//! 目标底图/小角缓存与矩形主体裁剪；缓存不接主题，不上传 GPU，不采样桌面。
use super::{PresentationPreviewPlan, PresentationRegion};
use crate::{
    capture::Image,
    capture_view::CaptureView,
    crop_mask::Polygon,
    geometry::Rect,
    pin_model::Placement,
    scene::{Scene, SourceIdentity, SourcePixels},
    selection_finish::SelectionFinishStyle,
    viewport::Viewport,
    Result,
};
use gpui::{point, px, Path, Pixels, RenderImage, Window};
use std::sync::Arc;

// 显示和导出预算分开：双4K加源采样余量允许超过导出32M，body+四角合计≤48M。
// 已发布帧最多192MB BGRA；原子候选或缩放预备底图与旧帧并存时该CPU缓存最多384MB。
// 原始图片、Scene、宿主/GPU纹理退休各自计入其它预算，不包含在此数值内。
const MAX_PREVIEW_PIXELS: u64 = 48_000_000;

#[derive(Clone)]
pub(crate) struct CornerTile {
    pub(crate) region: PresentationRegion,
    pub(crate) image: Arc<RenderImage>,
}

#[cfg(test)]
#[derive(Clone)]
struct CornerKey {
    plan: PresentationPreviewPlan,
    source_id: usize,
    scene: Arc<Scene>,
    mask: Option<Polygon>,
    annotations_visible: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct BodyKey {
    plan: PresentationPreviewPlan,
    source_id: usize,
    visible: Rect,
    rounded: bool,
}
#[cfg(test)]
impl CornerKey {
    #[cfg(test)]
    fn matches(&self, other: &Self) -> bool {
        self.matches_content(other) && Arc::ptr_eq(&self.scene, &other.scene)
    }
    #[cfg(test)]
    fn matches_content(&self, other: &Self) -> bool {
        self.plan == other.plan
            && self.source_id == other.source_id
            && self.mask == other.mask
            && self.annotations_visible == other.annotations_visible
    }
}

/// 缩放定位前按目标几何备好的底图，尚未上传；只被同键、同原图身份的下一次帧准备采用。
#[derive(Clone)]
struct StagedBody {
    key: BodyKey,
    source: SourceIdentity,
    tiles: Vec<CornerTile>,
}

/// 当前矩形底图与矢量内容组的完整绑定；多平面/独立光标也属于Source身份。
#[derive(Clone)]
struct GroupBinding {
    plan: PresentationPreviewPlan,
    source_id: usize,
    source: SourceIdentity,
    scene: Arc<Scene>,
    visible: Rect,
    annotations_visible: bool,
    mask: Option<Polygon>,
}

#[derive(Clone, Default)]
pub(crate) struct CornerCache {
    #[cfg(test)]
    key: Option<CornerKey>,
    tiles: Vec<CornerTile>,
    body_key: Option<BodyKey>,
    body_tiles: Vec<CornerTile>,
    rasters: Vec<(Arc<Image>, Arc<RenderImage>)>,
    group_binding: Option<GroupBinding>,
    body_source_identity: Option<SourceIdentity>,
    staged_body: Option<StagedBody>,
}

/// 仅在当前原图身份仍有效且body/角来自同一帧时给宿主一致的失败回退。
pub(crate) struct CachedPresentationFrame {
    pub(crate) plan: PresentationPreviewPlan,
    pub(crate) scene: Arc<Scene>,
    pub(crate) annotations_visible: bool,
    #[cfg(test)]
    pub(crate) visible: Rect,
    pub(crate) rasters: Vec<(Arc<Image>, Arc<RenderImage>)>,
}
impl CornerCache {
    /// 普通Pin保留原合同：后来编辑crop仅决定输出，显示不套新来源的裁剪mask。
    #[cfg(test)]
    pub(crate) fn prepare_group_frame(
        &mut self,
        plan: PresentationPreviewPlan,
        source_binding: (usize, &SourcePixels<'_>),
        scene: Arc<Scene>,
        visible: Rect,
        annotations_visible: bool,
        rasters: &[(Arc<Image>, Arc<RenderImage>)],
    ) -> Result<Vec<Arc<RenderImage>>> {
        self.prepare_group_frame_masked(
            plan,
            source_binding,
            scene,
            visible,
            annotations_visible,
            None,
            rasters,
        )
    }

    /// 原子准备矩形原像素+真实Scene+整体矢量mask绑定；不烘套索或圆角alpha。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_group_frame_masked(
        &mut self,
        plan: PresentationPreviewPlan,
        source_binding: (usize, &SourcePixels<'_>),
        scene: Arc<Scene>,
        visible: Rect,
        annotations_visible: bool,
        mask: Option<&Polygon>,
        rasters: &[(Arc<Image>, Arc<RenderImage>)],
    ) -> Result<Vec<Arc<RenderImage>>> {
        let (source_id, source) = source_binding;
        validate_group_preview_frame(plan, visible)?;
        if let Some(mask) = mask {
            validate_polygon_content_geometry(mask)?;
        }
        if let Some(error) = scene.first_error() {
            return Err(error.to_owned().into());
        }
        let identity = SourceIdentity::of(source);
        let mut staged = self.clone();
        // 第一平面RenderImage id相同也不能复用改变了第二平面/光标的底图。
        let mut retired = if staged.body_source_identity.as_ref() != Some(&identity) {
            staged.clear_body()
        } else {
            Vec::new()
        };
        // 预备底图在任何一次帧准备后都失效：同键同原图直接采用（与现采逐字节相同），否则退休。
        if let Some(prepared) = staged.staged_body.take() {
            let key = BodyKey {
                plan,
                source_id,
                visible,
                rounded: false,
            };
            if prepared.key == key && prepared.source == identity && staged.body_key != Some(key) {
                retired.extend(staged.clear_body());
                staged.body_key = Some(key);
                staged.body_tiles = prepared.tiles;
                crate::pin_handoff_smoke::mark("zoom_body_staged_reuse");
            } else {
                retired.extend(prepared.tiles.into_iter().map(|tile| tile.image));
            }
        }
        retired.extend(staged.prepare_body_pixels(plan, source_id, source, visible, false)?);
        staged.body_source_identity = Some(identity.clone());
        retired.extend(staged.clear_corners());
        #[cfg(test)]
        {
            staged.key = Some(CornerKey {
                plan,
                source_id,
                scene: scene.clone(),
                mask: mask.cloned(),
                annotations_visible,
            });
        }
        staged.group_binding = Some(GroupBinding {
            plan,
            source_id,
            source: identity,
            scene,
            visible,
            annotations_visible,
            mask: mask.cloned(),
        });
        let old_rasters = std::mem::replace(&mut staged.rasters, rasters.to_vec());
        retired.extend(
            old_rasters
                .into_iter()
                .map(|(_, image)| image)
                .filter(|old| !rasters.iter().any(|(_, current)| Arc::ptr_eq(old, current))),
        );
        *self = staged;
        Ok(retired)
    }

    /// 准备资格必须与当前Source、可见域、计划、Scene、显示开关和mask都一致。
    /// 此查询只是CPU准备资格；实际首显还必须由renderer成功Present写入marker。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn group_frame_matches(
        &self,
        plan: PresentationPreviewPlan,
        source_binding: (usize, &SourcePixels<'_>),
        scene: &Arc<Scene>,
        visible: Rect,
        annotations_visible: bool,
        mask: Option<&Polygon>,
    ) -> bool {
        self.group_binding.as_ref().is_some_and(|bound| {
            bound.plan == plan
                && bound.source_id == source_binding.0
                && bound.source == SourceIdentity::of(source_binding.1)
                && Arc::ptr_eq(&bound.scene, scene)
                && bound.visible == visible
                && bound.annotations_visible == annotations_visible
                && bound.mask.as_ref() == mask
                && self.body_key.is_some_and(|body| {
                    body.plan == plan
                        && body.source_id == source_binding.0
                        && body.visible == visible
                        && !body.rounded
                })
                && self.body_source_identity.as_ref() == Some(&bound.source)
                && !self.body_tiles.is_empty()
                && scene.first_error().is_none()
        })
    }

    /// 准备失败只允许完整的同Source/计划/可见域/mask旧帧继续显示；不授予新首显资格。
    pub(crate) fn cached_group_frame(
        &self,
        plan: PresentationPreviewPlan,
        source_binding: (usize, &SourcePixels<'_>),
        visible: Rect,
        mask: Option<&Polygon>,
    ) -> Option<CachedPresentationFrame> {
        let bound = self.group_binding.as_ref()?;
        self.group_frame_matches(
            plan,
            source_binding,
            &bound.scene,
            visible,
            bound.annotations_visible,
            mask,
        )
        .then(|| CachedPresentationFrame {
            plan: bound.plan,
            scene: bound.scene.clone(),
            annotations_visible: bound.annotations_visible,
            #[cfg(test)]
            visible: bound.visible,
            rasters: self.rasters.clone(),
        })
    }

    /// Arc克隆只构成候选，不拷贝旧像素；两组成功后才一次性发布并交回退休图片。
    #[cfg(test)]
    pub(crate) fn prepare_frame(
        &mut self,
        plan: PresentationPreviewPlan,
        source_binding: (usize, &SourcePixels<'_>),
        scene: Arc<Scene>,
        visible: Rect,
        annotations_visible: bool,
        rasters: &[(Arc<Image>, Arc<RenderImage>)],
    ) -> Result<Vec<Arc<RenderImage>>> {
        let (source_id, source) = source_binding;
        validate_preview_frame(plan, visible)?;
        let mut staged = self.clone();
        let mut retired = staged.prepare_body(plan, source_id, source, visible)?;
        retired.extend(staged.prepare(
            plan,
            source_id,
            scene,
            source,
            None,
            annotations_visible,
        )?);
        // 旧Scene的主体patch也属于完整帧；这里只克隆真实映射的Arc，不复制像素。
        let old_rasters = std::mem::replace(&mut staged.rasters, rasters.to_vec());
        retired.extend(
            old_rasters
                .into_iter()
                .map(|(_, preview)| preview)
                .filter(|old| !rasters.iter().any(|(_, current)| Arc::ptr_eq(old, current))),
        );
        *self = staged;
        Ok(retired)
    }
    #[cfg(test)]
    pub(crate) fn cached_frame(&self, source_id: usize) -> Option<CachedPresentationFrame> {
        let body = self.body_key?;
        let corner = self.key.as_ref()?;
        (body.source_id == source_id
            && corner.source_id == source_id
            && body.plan == corner.plan
            && corner.mask.is_none())
        .then(|| CachedPresentationFrame {
            plan: corner.plan,
            scene: corner.scene.clone(),
            annotations_visible: corner.annotations_visible,
            visible: body.visible,
            rasters: self.rasters.clone(),
        })
    }
    /// source_id 使用 RenderImage.id.0 等稳定身份，不能使用可能被重用的短寿命地址。
    /// 成功才替换旧缓存，返回旧纹理交宿主延迟 retire；失败保留旧缓存供调用者清理。
    #[cfg(test)]
    pub(crate) fn prepare(
        &mut self,
        plan: PresentationPreviewPlan,
        source_id: usize,
        scene: Arc<Scene>,
        source: &SourcePixels<'_>,
        mask: Option<&Polygon>,
        annotations_visible: bool,
    ) -> Result<Vec<Arc<RenderImage>>> {
        let key = CornerKey {
            plan,
            source_id,
            scene,
            mask: mask.cloned(),
            annotations_visible,
        };
        if self.key.as_ref().is_some_and(|old| old.matches(&key)) {
            return Ok(Vec::new());
        }
        // 仅Scene变化时核真实渲染资源；不把标注相等误当成下层采样效果未变化。
        // 四角共用一次网格范围扫描，中心自由笔的逐点草稿无需再栅格化四角。
        let changed_bounds = self
            .key
            .as_ref()
            .filter(|old| annotations_visible && old.matches_content(&key))
            .and_then(|old| changed_mesh_bounds(&old.scene, &key.scene));
        let empty = Scene::default();
        let scene = if annotations_visible {
            key.scene.as_ref()
        } else {
            &empty
        };
        let mut tiles = Vec::new();
        for region in corner_tile_regions(plan)? {
            if changed_bounds
                .as_ref()
                .is_some_and(|bounds| meshes_miss_region(bounds, plan, region))
            {
                if let Some(tile) = self.tiles.iter().find(|tile| tile.region == region) {
                    tiles.push(tile.clone());
                    continue;
                }
            }
            let raw = plan.render_corner(source, scene, mask, region)?;
            // GPUI Windows 与 CanvasSource 相同的 BGRA 字节约定；不做 premultiply/flatten。
            let pixels = image::RgbaImage::from_raw(raw.width, raw.height, raw.bgra)
                .ok_or("贴图圆角预览像素长度无效")?;
            tiles.push(CornerTile {
                region,
                image: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
            });
        }
        let retired = std::mem::replace(&mut self.tiles, tiles)
            .into_iter()
            .map(|tile| tile.image)
            .filter(|old| !self.tiles.iter().any(|tile| Arc::ptr_eq(old, &tile.image)))
            .collect();
        // 即使四角全部复用，也绑定本次Scene，保证主体/角与失败回退属于同一帧。
        self.key = Some(key);
        Ok(retired)
    }
    pub(crate) fn clear(&mut self) -> Vec<Arc<RenderImage>> {
        self.group_binding = None;
        let mut retired = self.clear_corners();
        retired.extend(self.clear_body());
        retired.extend(self.clear_staged_body());
        retired.extend(
            std::mem::take(&mut self.rasters)
                .into_iter()
                .map(|(_, image)| image),
        );
        retired
    }
    fn clear_corners(&mut self) -> Vec<Arc<RenderImage>> {
        #[cfg(test)]
        {
            self.key = None;
        }
        std::mem::take(&mut self.tiles)
            .into_iter()
            .map(|tile| tile.image)
            .collect()
    }
    fn clear_staged_body(&mut self) -> Vec<Arc<RenderImage>> {
        self.staged_body
            .take()
            .map(|staged| staged.tiles.into_iter().map(|tile| tile.image).collect())
            .unwrap_or_default()
    }
    /// 定位前为目标几何采样底图，不改动当前已发布的帧；当前或已预备的同键底图不重复采样。
    pub(crate) fn stage_body(
        &mut self,
        plan: PresentationPreviewPlan,
        source_id: usize,
        source: &SourcePixels<'_>,
        visible: Rect,
    ) -> Result<Vec<Arc<RenderImage>>> {
        validate_group_preview_frame(plan, visible)?;
        let key = BodyKey {
            plan,
            source_id,
            visible,
            rounded: false,
        };
        let identity = SourceIdentity::of(source);
        if self.body_key == Some(key) && self.body_source_identity.as_ref() == Some(&identity) {
            return Ok(self.clear_staged_body());
        }
        if self
            .staged_body
            .as_ref()
            .is_some_and(|staged| staged.key == key && staged.source == identity)
        {
            return Ok(Vec::new());
        }
        // 先退休旧预备，CPU 上最多只有当前帧和一份预备底图。
        let retired = self.clear_staged_body();
        crate::pin_handoff_smoke::mark("zoom_prestage_begin");
        let tiles = sample_body_tiles(plan, source, visible, false)?;
        crate::pin_handoff_smoke::mark("zoom_prestage_end");
        self.staged_body = Some(StagedBody {
            key,
            source: identity,
            tiles,
        });
        Ok(retired)
    }
    fn clear_body(&mut self) -> Vec<Arc<RenderImage>> {
        self.body_source_identity = None;
        self.body_key = None;
        std::mem::take(&mut self.body_tiles)
            .into_iter()
            .map(|tile| tile.image)
            .collect()
    }
    /// 原始底图按客户区目标格线性采样；标注变化不会清空或重新生成 body。
    #[cfg(test)]
    pub(crate) fn prepare_body(
        &mut self,
        plan: PresentationPreviewPlan,
        source_id: usize,
        source: &SourcePixels<'_>,
        visible: Rect,
    ) -> Result<Vec<Arc<RenderImage>>> {
        self.prepare_body_pixels(plan, source_id, source, visible, true)
    }

    fn prepare_body_pixels(
        &mut self,
        plan: PresentationPreviewPlan,
        source_id: usize,
        source: &SourcePixels<'_>,
        visible: Rect,
        rounded: bool,
    ) -> Result<Vec<Arc<RenderImage>>> {
        let key = BodyKey {
            plan,
            source_id,
            visible,
            rounded,
        };
        if self.body_key == Some(key) {
            return Ok(Vec::new());
        }
        crate::pin_handoff_smoke::mark("zoom_body_sample_begin");
        let tiles = sample_body_tiles(plan, source, visible, rounded)?;
        crate::pin_handoff_smoke::mark("zoom_body_sample_end");
        let retired = self.clear_body();
        self.body_key = Some(key);
        self.body_tiles = tiles;
        Ok(retired)
    }
    /// 暴露强引用供 active texture reconcile 与实际小角绘制，宿主不克隆 CPU 像素。
    #[cfg(test)]
    pub(crate) fn tiles(&self) -> &[CornerTile] {
        &self.tiles
    }
    pub(crate) fn body_tiles(&self) -> &[CornerTile] {
        &self.body_tiles
    }
    /// 失败回退旧Scene时保留相应主体patch纹理；宿主current与retired过滤共用。
    pub(crate) fn raster_previews(&self) -> &[(Arc<Image>, Arc<RenderImage>)] {
        &self.rasters
    }
}

/// 只有真实资源与参与显示的字段一致才认为层未变化；其它变化只接受局部矢量网格。
#[cfg(test)]
fn shared_layer(before: &crate::scene::Layer, after: &crate::scene::Layer) -> bool {
    use crate::scene::Layer;
    match (before, after) {
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
                mesh: am,
                path: ap,
                segments: ag,
                style: ast,
            },
            Layer::Vector {
                arrow: b,
                mesh: bm,
                path: bp,
                segments: bg,
                style: bst,
            },
        ) => {
            a == b
                && ast == bst
                && Arc::ptr_eq(am, bm)
                && Arc::ptr_eq(ap, bp)
                && Arc::ptr_eq(ag, bg)
        }
        (
            Layer::Highlighter {
                mesh: am,
                path: ap,
                segments: ag,
                style: ast,
                tiles: at,
            },
            Layer::Highlighter {
                mesh: bm,
                path: bp,
                segments: bg,
                style: bst,
                tiles: bt,
            },
        ) => {
            ast == bst
                && Arc::ptr_eq(am, bm)
                && Arc::ptr_eq(ap, bp)
                && Arc::ptr_eq(ag, bg)
                && Arc::ptr_eq(at, bt)
        }
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

#[cfg(test)]
fn changed_mesh_bounds(before: &Scene, after: &Scene) -> Option<Vec<crate::annotation::Bounds>> {
    if before.layers.len() != after.layers.len()
        || before.layers.len() != before.annotations.len()
        || after.layers.len() != after.annotations.len()
        || before.first_error().is_some()
        || after.first_error().is_some()
    {
        return None;
    }
    let mut bounds = Vec::new();
    for ((old, new), (old_annotation, new_annotation)) in before
        .layers
        .iter()
        .zip(&after.layers)
        .zip(before.annotations.iter().zip(&after.annotations))
    {
        if old_annotation == new_annotation && shared_layer(old, new) {
            continue;
        }
        for layer in [old, new] {
            let mesh = match layer {
                crate::scene::Layer::Vector { mesh, .. }
                | crate::scene::Layer::Highlighter { mesh, .. } => mesh,
                // 栅格/恢复/聚光灯/文字/效果变化可能传播下层采样，保持全量重绘。
                _ => return None,
            };
            if mesh.vertices.is_empty() || mesh.edges.is_empty() {
                continue;
            }
            let mut left = f32::INFINITY;
            let mut top = f32::INFINITY;
            let mut right = f32::NEG_INFINITY;
            let mut bottom = f32::NEG_INFINITY;
            for p in &mesh.vertices {
                if !p.x.is_finite() || !p.y.is_finite() {
                    return None;
                }
                left = left.min(p.x);
                top = top.min(p.y);
                right = right.max(p.x);
                bottom = bottom.max(p.y);
            }
            let (width, height) = (right - left, bottom - top);
            if !width.is_finite() || !height.is_finite() {
                return None;
            }
            bounds.push(crate::annotation::Bounds {
                x: left,
                y: top,
                width,
                height,
            });
        }
    }
    Some(bounds)
}

#[cfg(test)]
fn meshes_miss_region(
    bounds: &[crate::annotation::Bounds],
    plan: PresentationPreviewPlan,
    region: PresentationRegion,
) -> bool {
    // 完全沿用render_content_region的f32投影与绝对相位，不能用整数源ROI近似。
    let projection = crate::output_projection::OutputProjection {
        source: plan.content,
        width: region.width,
        height: region.height,
        sx: plan.target.sx as f32,
        sy: plan.target.sy as f32,
        origin: crate::annotation::Point {
            x: (f64::from(region.left) - plan.target.origin.x) as f32,
            y: (f64::from(region.top) - plan.target.origin.y) as f32,
        },
    };
    if !projection.sx.is_finite()
        || !projection.sy.is_finite()
        || projection.sx <= 0.
        || projection.sy <= 0.
        || !projection.origin.x.is_finite()
        || !projection.origin.y.is_finite()
    {
        return false;
    }
    // 真实mesh_region留1目标像素AA；再保守加1源像素，覆盖线性采样邻域。
    // 底图身份未变、Highlighter只有网格覆盖内写色；这个范围只会多重绘，不会漏更新。
    let padding = 1. + projection.sx.max(projection.sy);
    bounds
        .iter()
        .all(|&bounds| projection.bounds(bounds, padding).is_none())
}

fn sample_body_tiles(
    plan: PresentationPreviewPlan,
    source: &SourcePixels<'_>,
    visible: Rect,
    rounded: bool,
) -> Result<Vec<CornerTile>> {
    let mut tiles = Vec::new();
    let empty = Scene::default();
    for region in body_tile_regions(plan, visible)? {
        let raw = if rounded {
            plan.render_content_region(source, &empty, None, region)?
        } else {
            plan.render_unclipped_background(source, region)?
        };
        let pixels = image::RgbaImage::from_raw(raw.width, raw.height, raw.bgra)
            .ok_or("贴图目标底图预览像素长度无效")?;
        tiles.push(CornerTile {
            region,
            image: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        });
    }
    Ok(tiles)
}

/// 可见 source ROI 只决定目标块覆盖范围，插值始终借用 full stride 和绝对相位。
fn body_tile_regions(
    plan: PresentationPreviewPlan,
    visible: Rect,
) -> Result<Vec<PresentationRegion>> {
    if !visible.valid(u32::MAX, u32::MAX) || visible.intersection(plan.content) != Some(visible) {
        return Err("贴图可见原图范围无效".into());
    }
    let content = plan.content_bounds();
    let raster = super::raster_bounds(super::PresentationBounds {
        left: content.left + f64::from(visible.x - plan.content.x) * plan.target.sx,
        top: content.top + f64::from(visible.y - plan.content.y) * plan.target.sy,
        width: f64::from(visible.width) * plan.target.sx,
        height: f64::from(visible.height) * plan.target.sy,
    })?;
    // body先核48M上界；实际帧还计入四角，用户导出whole仍保持32M。
    if u64::from(raster.width) * u64::from(raster.height) > MAX_PREVIEW_PIXELS {
        return Err("贴图可见目标底图超过 4800 万预览像素，请减小窗口或缩放".into());
    }
    let mut regions = Vec::new();
    for y in (0..raster.height).step_by(super::PREVIEW_TILE_SIDE as usize) {
        for x in (0..raster.width).step_by(super::PREVIEW_TILE_SIDE as usize) {
            regions.push(PresentationRegion {
                left: raster.left + x as i32,
                top: raster.top + y as i32,
                width: super::PREVIEW_TILE_SIDE.min(raster.width - x),
                height: super::PREVIEW_TILE_SIDE.min(raster.height - y),
            });
        }
    }
    Ok(regions)
}

#[cfg(test)]
fn corner_tile_regions(plan: PresentationPreviewPlan) -> Result<Vec<PresentationRegion>> {
    let regions = plan.corner_regions();
    let mut pixels = 0_u64;
    for &region in &regions {
        super::validate_preview_region(region)?;
        pixels += u64::from(region.width) * u64::from(region.height);
    }
    if pixels > MAX_PREVIEW_PIXELS {
        return Err("贴图四角目标预览超过 4800 万像素，请减小窗口或圆角".into());
    }
    Ok(regions)
}

/// 新呈现组仅缓存截图底图的矩形采样，不缓存圆角或标注像素。
fn validate_group_preview_frame(plan: PresentationPreviewPlan, visible: Rect) -> Result<()> {
    let regions = body_tile_regions(plan, visible)?;
    let pixels = regions
        .iter()
        .map(|r| u64::from(r.width) * u64::from(r.height))
        .sum::<u64>();
    if pixels > MAX_PREVIEW_PIXELS {
        return Err("贴图目标底图超过4800万预览像素".into());
    }
    Ok(())
}

/// 旧CPU圆角参考模型保留，不再用于产品呈现。
/// 主体和四角有单独像素分配，重叠部分也计入真实缓存总额。
#[cfg(test)]
fn validate_preview_frame(plan: PresentationPreviewPlan, visible: Rect) -> Result<()> {
    let body = body_tile_regions(plan, visible)?;
    let corners = corner_tile_regions(plan)?;
    let pixels = body
        .iter()
        .chain(corners.iter())
        .map(|region| u64::from(region.width) * u64::from(region.height))
        .sum::<u64>();
    if pixels > MAX_PREVIEW_PIXELS {
        return Err("贴图主体与四角目标预览合计超过 4800 万像素，请减小窗口或圆角".into());
    }
    Ok(())
}

/// 与实际可见ROI共用虚拟桌面交集和2px源余量；不调用Win32、不读取像素。
pub(crate) fn preview_visible_source_region(
    content: Rect,
    viewport: Viewport,
    placement: Placement,
    desktop: Placement,
) -> Result<Rect> {
    if content.x != 0
        || content.y != 0
        || !content.valid(u32::MAX, u32::MAX)
        || !viewport.zoom.is_finite()
        || viewport.zoom <= 0.
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
        || [placement, desktop]
            .into_iter()
            .any(|rect| !valid_native_rect(rect))
    {
        return Err("贴图预览可见几何无效".into());
    }
    let x0 = (i64::from(desktop.x) - i64::from(placement.x)).clamp(0, i64::from(placement.width));
    let y0 = (i64::from(desktop.y) - i64::from(placement.y)).clamp(0, i64::from(placement.height));
    let x1 = (i64::from(desktop.x) + i64::from(desktop.width) - i64::from(placement.x))
        .clamp(x0, i64::from(placement.width));
    let y1 = (i64::from(desktop.y) + i64::from(desktop.height) - i64::from(placement.y))
        .clamp(y0, i64::from(placement.height));
    let start = viewport.image_point(crate::annotation::Point {
        x: x0 as f32,
        y: y0 as f32,
    });
    let end = viewport.image_point(crate::annotation::Point {
        x: x1 as f32,
        y: y1 as f32,
    });
    // f32端点在极大普通画布上可能向上舍入；最后核整数边界，保证x+1/y+1不溢出。
    let x = ((start.x.floor() - 2.).clamp(0., content.width.saturating_sub(1) as f32) as u32)
        .min(content.width - 1);
    let y = ((start.y.floor() - 2.).clamp(0., content.height.saturating_sub(1) as f32) as u32)
        .min(content.height - 1);
    let right = (end.x.ceil() + 2.).clamp((x + 1) as f32, content.width as f32) as u32;
    let bottom = (end.y.ceil() + 2.).clamp((y + 1) as f32, content.height as f32) as u32;
    Ok(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// 几何变更前的纯预校验：与host contain、真实2px源ROI及预览48M/4096分块共用。
#[cfg(test)]
pub(crate) fn validate_preview_geometry(
    content: (u32, u32),
    style: SelectionFinishStyle,
    placement: Placement,
    desktop: Placement,
) -> Result<()> {
    validate_preview_geometry_with_sizing(
        content,
        style,
        placement,
        desktop,
        crate::pin_model::PinWindowSizing::General,
    )
}

pub(crate) fn validate_preview_geometry_with_sizing(
    content: (u32, u32),
    style: SelectionFinishStyle,
    placement: Placement,
    desktop: Placement,
    sizing: crate::pin_model::PinWindowSizing,
) -> Result<()> {
    if placement.width == 0
        || placement.height == 0
        || placement.width > crate::pin_model::MAX_PIN_WINDOW_SIDE
        || placement.height > crate::pin_model::MAX_PIN_WINDOW_SIDE
    {
        return Err("贴图客户区为空或超过 16384 物理像素".into());
    }
    let style = style.normalized();
    crate::pin_presentation_host::window_size(content, Some(style))?;
    let viewport = crate::pin_presentation_host::content_viewport_with_sizing(
        content,
        (placement.width, placement.height),
        Some(style),
        sizing,
    );
    let plan = PresentationPreviewPlan::new(
        Rect::full(content.0, content.1),
        style,
        super::PhysicalPoint {
            x: f64::from(viewport.offset.x),
            y: f64::from(viewport.offset.y),
        },
        f64::from(viewport.zoom),
    )?;
    let visible = preview_visible_source_region(plan.content, viewport, placement, desktop)?;
    validate_group_preview_frame(plan, visible)?;
    Ok(())
}

fn valid_native_rect(rect: Placement) -> bool {
    rect.width > 0
        && rect.height > 0
        && i64::from(rect.x) + i64::from(rect.width) <= i64::from(i32::MAX)
        && i64::from(rect.y) + i64::from(rect.height) <= i64::from(i32::MAX)
}

/// 系统拓扑改变时给宿主一个已通过同一预算的缩小客户区；不移动x/y，不改源或style。
/// 两轴使用同一有理比例，仅客户区整数尺寸最后floor/min1；内容仍由host统一contain。
/// 返回已核验候选，不保证离散栅格/采样余量下的极限最大尺寸，也不调用窗口API。
#[cfg(test)]
pub(crate) fn fit_preview_placement(
    content: (u32, u32),
    style: SelectionFinishStyle,
    placement: Placement,
    desktop: Placement,
) -> Result<Placement> {
    fit_preview_placement_with_sizing(
        content,
        style,
        placement,
        desktop,
        crate::pin_model::PinWindowSizing::General,
    )
}

pub(crate) fn fit_preview_placement_with_sizing(
    content: (u32, u32),
    style: SelectionFinishStyle,
    placement: Placement,
    desktop: Placement,
    sizing: crate::pin_model::PinWindowSizing,
) -> Result<Placement> {
    if validate_preview_geometry_with_sizing(content, style, placement, desktop, sizing).is_ok() {
        return Ok(placement);
    }
    // 源/outer预算和无效全局坐标不能靠缩小预览掩盖；它们与显示拓扑无关。
    let effective = style.normalized();
    crate::pin_presentation_host::window_size(content, Some(effective))?;
    if !valid_native_rect(placement) || !valid_native_rect(desktop) {
        return Err("贴图预览调整的窗口或桌面范围无效".into());
    }
    let extent = placement.width.max(placement.height);
    let candidate = |side: u32| -> Result<Placement> {
        let dimension = |original: u32| -> Result<u32> {
            let value = u64::from(original)
                .checked_mul(u64::from(side))
                .and_then(|v| v.checked_div(u64::from(extent)))
                .ok_or("贴图等比预览调整尺寸溢出")?;
            Ok(u32::try_from(value.max(1))?)
        };
        Ok(Placement {
            x: placement.x,
            y: placement.y,
            width: dimension(placement.width)?,
            height: dimension(placement.height)?,
        })
    };
    let mut rejected = extent;
    let mut accepted = (extent / 2).max(1);
    let mut fitted = loop {
        let next = candidate(accepted)?;
        match validate_preview_geometry_with_sizing(content, style, next, desktop, sizing) {
            Ok(()) => break next,
            Err(error) if accepted == 1 => return Err(error),
            Err(_) => {
                rejected = accepted;
                accepted = (accepted / 2).max(1);
            }
        }
    };
    // u32有界区间至多32次；只以实际通过预检的候选更新fitted，正确性不依赖单调性。
    while rejected - accepted > 1 {
        let middle = accepted + (rejected - accepted) / 2;
        let next = candidate(middle)?;
        if validate_preview_geometry_with_sizing(content, style, next, desktop, sizing).is_ok() {
            fitted = next;
            accepted = middle;
        } else {
            rejected = middle;
        }
    }
    Ok(fitted)
}

/// 无状态重放一次。分数边缘与圆角都交给外层GPU内容组统一覆盖，
/// 不对Scene再套硬矩形mask，否则像素中心在边缘外的部分覆盖会被提前丢弃。
pub(crate) fn paint_content_regions(
    _plan: Option<PresentationPreviewPlan>,
    _origin: gpui::Point<Pixels>,
    window: &mut Window,
    mut paint: impl FnMut(&mut Window),
) {
    paint(window);
}

fn validate_polygon_content_geometry(mask: &Polygon) -> Result<()> {
    let bounds = mask.bounds;
    if mask.points.len() < 3
        || mask.triangles.is_empty()
        || ![bounds.x, bounds.y, bounds.width, bounds.height]
            .iter()
            .all(|v| v.is_finite())
        || bounds.width <= 0.
        || bounds.height <= 0.
        || !mask
            .points
            .iter()
            .all(|p| p.x.is_finite() && p.y.is_finite())
        || !mask
            .triangles
            .iter()
            .flatten()
            .all(|p| p.x.is_finite() && p.y.is_finite())
    {
        return Err("贴图整体套索几何无效".into());
    }
    Ok(())
}

type PolygonContentTriangle = [(gpui::Point<Pixels>, gpui::Point<f32>); 3];

/// 实际GPU路径的投影输入。流式保留NonZero三角片，不生成CPU alpha平面。
fn polygon_content_triangles(
    mask: &Polygon,
    viewport: Viewport,
    dpi: f32,
) -> Result<impl Iterator<Item = Result<PolygonContentTriangle>> + '_> {
    validate_polygon_content_geometry(mask)?;
    if !dpi.is_finite()
        || dpi <= 0.
        || !viewport.zoom.is_finite()
        || viewport.zoom <= 0.
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
        || mask.triangles.is_empty()
    {
        return Err("贴图整体套索视口或几何无效".into());
    }
    let screen = move |p: crate::annotation::Point| -> Result<gpui::Point<Pixels>> {
        let at = viewport.window_point(p);
        if !p.x.is_finite() || !p.y.is_finite() || !at.x.is_finite() || !at.y.is_finite() {
            return Err("贴图整体套索顶点非有限值".into());
        }
        Ok(point(px(at.x / dpi), px(at.y / dpi)))
    };
    let uv = point(0., 1.);
    Ok(mask
        .triangles
        .iter()
        .map(move |&[a, b, c]| Ok([(screen(a)?, uv), (screen(b)?, uv), (screen(c)?, uv)])))
}

/// 直接消费文档既有NonZero无重叠三角片，不构造first-point fan，也不生成CPU alpha图。
fn polygon_content_path(mask: &Polygon, viewport: Viewport, dpi: f32) -> Result<Path<Pixels>> {
    let mut triangles = polygon_content_triangles(mask, viewport, dpi)?;
    let first = triangles
        .next()
        .transpose()?
        .ok_or("贴图整体套索没有三角片")?;
    let mut path = Path::new(first[0].0);
    for triangle in std::iter::once(Ok(first)).chain(triangles) {
        let [(a, ua), (b, ub), (c, uc)] = triangle?;
        path.push_triangle((a, b, c), (ua, ub, uc));
    }
    Ok(path)
}

impl CaptureView {
    /// 第2阶段完整源域与固定显示裁剪分离；原点仍是截图世界坐标。
    pub(crate) fn pin_presentation_content_rect(&self) -> Rect {
        self.pin_content_rect.unwrap_or(self.source.full())
    }
    pub(crate) fn pin_presentation_group_mask(&self) -> Option<&Polygon> {
        // 普通Pin后来crop沿旧显示行为；完整Capture来源需要真实整体mask。
        self.pin_content_rect
            .and(self.editor.document.crop_mask.as_ref())
    }
    pub(crate) fn pin_presentation_group_path(
        &self,
        window: &Window,
    ) -> Result<Option<Path<Pixels>>> {
        self.pin_presentation_group_mask()
            .map(|mask| {
                polygon_content_path(mask, self.canvas_viewport(window), window.scale_factor())
            })
            .transpose()
    }
    pub(crate) fn pin_presentation_group_frame_matches(
        &self,
        window: &Window,
        plan: PresentationPreviewPlan,
        scene: &Arc<Scene>,
        annotations_visible: bool,
    ) -> bool {
        let Some(preview) = self.previews.first() else {
            return false;
        };
        let source_id = self.source.original(preview).id.0;
        let visible = crate::pin_annotation::shared_preview_region(self, window)
            .intersection(self.pin_presentation_content_rect())
            .unwrap_or(self.pin_presentation_content_rect());
        self.pin_presentation_preview.group_frame_matches(
            plan,
            (source_id, &self.source_pixels()),
            scene,
            visible,
            annotations_visible,
            self.pin_presentation_group_mask(),
        )
    }
    pub(crate) fn pin_presentation_retained_group_frame(
        &self,
        window: &Window,
        plan: PresentationPreviewPlan,
    ) -> Option<CachedPresentationFrame> {
        let preview = self.previews.first()?;
        let source_id = self.source.original(preview).id.0;
        let visible = crate::pin_annotation::shared_preview_region(self, window)
            .intersection(self.pin_presentation_content_rect())
            .unwrap_or(self.pin_presentation_content_rect());
        self.pin_presentation_preview.cached_group_frame(
            plan,
            (source_id, &self.source_pixels()),
            visible,
            self.pin_presentation_group_mask(),
        )
    }
    pub(crate) fn pin_presentation_preview_plan(
        &self,
        window: &Window,
        style: SelectionFinishStyle,
    ) -> Result<PresentationPreviewPlan> {
        self.pin_presentation_plan_for(self.canvas_viewport(window), style)
    }
    fn pin_presentation_plan_for(
        &self,
        viewport: Viewport,
        style: SelectionFinishStyle,
    ) -> Result<PresentationPreviewPlan> {
        let content = self.pin_presentation_content_rect();
        let at = viewport.window_point(crate::annotation::Point {
            x: content.x as f32,
            y: content.y as f32,
        });
        PresentationPreviewPlan::new(
            content,
            style,
            super::PhysicalPoint {
                x: f64::from(at.x),
                y: f64::from(at.y),
            },
            f64::from(viewport.zoom),
        )
    }
    /// 改尺寸定位前按目标窗口矩形预备底图，使改尺寸后的首帧不再等CPU采样。
    /// 与实际帧同一视口/计划/可见域公式；几何或原图对不上时预备作废，下一帧照常现采。
    pub(crate) fn prestage_pin_presentation_body(
        &mut self,
        window: &Window,
        placement: Placement,
    ) -> Result<()> {
        let style = self
            .pin_presentation_style
            .filter(|_| self.pin_mode && self.source.solid_fill().is_none());
        let Some(style) = style else {
            return Ok(());
        };
        // GPUI 由物理客户区除以DPI得逻辑视口，canvas_viewport 再乘回取整；这里走同一换算。
        let dpi = window.scale_factor();
        let host = |side: u32| ((side as f32 / dpi) * dpi).round() as u32;
        let content = self.pin_presentation_content_rect();
        let viewport = crate::pin_presentation_host::source_content_viewport(
            content,
            (host(placement.width), host(placement.height)),
            self.pin_presentation_style,
            self.pin_window_sizing,
        );
        let plan = self.pin_presentation_plan_for(viewport, style)?;
        let preview = self.previews.first().ok_or("截图贴图没有原始像素预览")?;
        let source_id = self.source.original(preview).id.0;
        let visible = crate::pin_annotation::shared_preview_region_at(self, viewport, placement)
            .intersection(content)
            .unwrap_or(content);
        let mut cache = std::mem::take(&mut self.pin_presentation_preview);
        let result = cache.stage_body(plan, source_id, &self.source_pixels(), visible);
        self.pin_presentation_preview = cache;
        self.retired_raster_previews.extend(result?);
        Ok(())
    }
    /// 目标底图按实际Source/视口复用；Scene与整体矢量mask独立绑定，不生成内容角图。
    pub(crate) fn prepare_pin_presentation_preview(
        &mut self,
        window: &Window,
        scene: &Arc<Scene>,
        annotations_visible: bool,
    ) -> Result<Option<PresentationPreviewPlan>> {
        let style = self
            .pin_presentation_style
            .filter(|_| self.pin_mode && self.source.solid_fill().is_none());
        let Some(style) = style else {
            self.retired_raster_previews
                .extend(self.pin_presentation_preview.clear());
            return Ok(None);
        };
        let prepared = (|| -> Result<_> {
            let content = self.pin_presentation_content_rect();
            let plan = self.pin_presentation_preview_plan(window, style)?;
            let preview = self.previews.first().ok_or("截图贴图没有原始像素预览")?;
            let source_id = self.source.original(preview).id.0;
            // 所有 styled raster 都走同一预乘目标采样；GPU straight 线性不能替代此路径。
            let visible = crate::pin_annotation::shared_preview_region(self, window)
                .intersection(content)
                .unwrap_or(content);
            Ok((plan, source_id, visible))
        })();
        let (plan, source_id, visible) = prepared?;
        // 暂时移出缓存，使 SourcePixels 借用不与 &mut CaptureView 字段重叠。
        let mut cache = std::mem::take(&mut self.pin_presentation_preview);
        let result = {
            let pixels = self.source_pixels();
            // 原像素完整保留；新Capture来源的套索作为整组GPU覆盖绑定。
            cache.prepare_group_frame_masked(
                plan,
                (source_id, &pixels),
                scene.clone(),
                visible,
                annotations_visible,
                self.pin_presentation_group_mask(),
                &self.raster_previews,
            )
        };
        match result {
            Ok(retired) => {
                self.retired_raster_previews.extend(retired);
                self.pin_presentation_preview = cache;
                Ok(Some(plan))
            }
            Err(error) => {
                // 候选从未发布，旧body/角及Scene仍构成同一有效帧，不能因失败清空。
                self.pin_presentation_preview = cache;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        capture::Image,
        geometry::Rect,
        pin_presentation::PhysicalPoint,
        scene::PixelPlane,
        selection_finish::SelectionFinishStyle,
    };

    fn raw() -> Image {
        Image {
            width: 31,
            height: 25,
            bgra: [23, 45, 67, 97].repeat(31 * 25),
            readback_ms: 0.,
        }
    }
    fn source(raw: &Image) -> SourcePixels<'_> {
        let canvas = Rect::full(raw.width, raw.height);
        SourcePixels {
            canvas,
            fill: None,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &raw.bgra,
            }],
            overlay: None,
        }
    }
    fn plan() -> PresentationPreviewPlan {
        PresentationPreviewPlan::new(
            Rect::full(31, 25),
            SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 5.25,
                ..Default::default()
            },
            PhysicalPoint { x: 3.25, y: 5.75 },
            1.375,
        )
        .unwrap()
    }

    #[test]
    fn actual_corner_cache_reuses_stable_scene_source_and_fractional_geometry() {
        let raw = raw();
        let source = source(&raw);
        let scene = Arc::new(Scene::default());
        let mut cache = CornerCache::default();
        assert!(cache
            .prepare(plan(), 7, scene.clone(), &source, None, true)
            .unwrap()
            .is_empty());
        let before: Vec<_> = cache
            .tiles()
            .iter()
            .map(|tile| tile.image.clone())
            .collect();
        assert_eq!(before.len(), 4);
        assert!(cache
            .prepare(plan(), 7, scene, &source, None, true)
            .unwrap()
            .is_empty());
        for (before, tile) in before.iter().zip(cache.tiles()) {
            assert!(Arc::ptr_eq(before, &tile.image));
        }
        assert!(cache
            .tiles()
            .iter()
            .flat_map(|tile| tile.image.as_bytes(0).unwrap().chunks_exact(4))
            .any(|p| p[3] > 0 && p[3] < 97));
        assert!(raw.bgra.chunks_exact(4).all(|p| p == [23, 45, 67, 97]));
    }

    fn assert_corners_match_fresh_render(
        cache: &CornerCache,
        plan: PresentationPreviewPlan,
        source: &SourcePixels<'_>,
        scene: &Scene,
    ) {
        for tile in cache.tiles() {
            let fresh = plan
                .render_corner(source, scene, None, tile.region)
                .unwrap();
            assert_eq!(tile.image.as_bytes(0).unwrap(), fresh.bgra);
        }
    }

    #[test]
    fn actual_corner_cache_reuses_center_draft_pen_and_highlighter_with_current_frame_pixels() {
        let mut raw = raw();
        for (index, pixel) in raw.bgra.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[
                (index * 7) as u8,
                (index * 13) as u8,
                (index * 19) as u8,
                [0, 97, 128, 255][index % 4],
            ]);
        }
        let source = source(&raw);
        let plan = plan();
        for tool in [Tool::Pen, Tool::Highlighter] {
            let mut prefix = Annotation::new(
                Tool::Rectangle,
                Point { x: 0.5, y: 0.5 },
                Style {
                    filled: true,
                    color: 0x00ff00,
                    ..Default::default()
                },
            );
            prefix.update(Point { x: 4.5, y: 4.5 });
            let mut draft = Annotation::new(
                tool,
                Point { x: 11., y: 11. },
                Style {
                    width: 1.5,
                    highlighter_blend: crate::annotation::HighlighterBlend::Multiply,
                    ..Default::default()
                },
            );
            draft.update(Point { x: 13., y: 11. });
            let mut scene = Arc::new(Scene::default().updated_with_draft(
                &[prefix.clone(), draft.clone()],
                &source,
                Some(1),
            ));
            assert!(scene.first_error().is_none());
            let mut cache = CornerCache::default();
            cache
                .prepare_frame(plan, (7, &source), scene.clone(), source.canvas, true, &[])
                .unwrap();
            let before: Vec<_> = cache
                .tiles()
                .iter()
                .map(|tile| tile.image.clone())
                .collect();
            let body = cache.body_tiles()[0].image.clone();
            for point in [
                Point { x: 15., y: 12. },
                Point { x: 17., y: 13. },
                Point { x: 16., y: 15. },
                Point { x: 13., y: 14. },
            ] {
                draft.update(point);
                let next = Arc::new(scene.updated_with_draft(
                    &[prefix.clone(), draft.clone()],
                    &source,
                    Some(1),
                ));
                assert!(!Arc::ptr_eq(&scene, &next));
                assert!(next.first_error().is_none());
                assert!(cache
                    .prepare_frame(plan, (7, &source), next.clone(), source.canvas, true, &[])
                    .unwrap()
                    .is_empty());
                for (before, tile) in before.iter().zip(cache.tiles()) {
                    assert!(Arc::ptr_eq(before, &tile.image));
                }
                assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
                assert!(Arc::ptr_eq(&cache.cached_frame(7).unwrap().scene, &next));
                assert_corners_match_fresh_render(&cache, plan, &source, &next);
                scene = next;
            }
            let retired = cache.clear();
            assert_eq!(retired.len(), 5);
            for image in before.iter().chain(std::iter::once(&body)) {
                assert_eq!(
                    retired.iter().filter(|old| Arc::ptr_eq(old, image)).count(),
                    1
                );
            }
        }
    }

    #[test]
    fn actual_corner_cache_updates_touching_and_wide_meshes_at_fractional_scales() {
        let raw = raw();
        let source = source(&raw);
        for scale in [0.625, 1.375, 2.625] {
            let plan = PresentationPreviewPlan::new(
                source.canvas,
                plan().style(),
                PhysicalPoint { x: 3.25, y: 5.75 },
                scale,
            )
            .unwrap();
            let mut mark = Annotation::new(
                Tool::Pen,
                Point { x: 15., y: 12. },
                Style {
                    width: 1.5,
                    ..Default::default()
                },
            );
            mark.update(Point { x: 16., y: 13. });
            let mut scene =
                Arc::new(Scene::default().updated_with_draft(&[mark.clone()], &source, Some(0)));
            let mut cache = CornerCache::default();
            cache
                .prepare(plan, 7, scene.clone(), &source, None, true)
                .unwrap();
            for (points, width) in [
                ([Point { x: 2.75, y: 2.25 }, Point { x: 4., y: 3. }], 1.5),
                ([Point { x: 8., y: 7.75 }, Point { x: 9., y: 8.75 }], 1.5),
                // 控制点在角块外，实际粗描边网格已触角；不能只看控制点或中心线。
                ([Point { x: 8., y: 7.75 }, Point { x: 9., y: 8.75 }], 14.5),
            ] {
                let before: Vec<_> = cache
                    .tiles()
                    .iter()
                    .map(|tile| tile.image.clone())
                    .collect();
                let before_pixels = before[0].as_bytes(0).unwrap().to_vec();
                mark.points = points.to_vec();
                mark.style.width = width;
                scene = Arc::new(scene.updated_with_draft(&[mark.clone()], &source, Some(0)));
                assert!(scene.first_error().is_none());
                let retired = cache
                    .prepare(plan, 7, scene.clone(), &source, None, true)
                    .unwrap();
                assert!(!Arc::ptr_eq(&before[0], &cache.tiles()[0].image));
                assert_ne!(before_pixels, cache.tiles()[0].image.as_bytes(0).unwrap());
                let changed = before
                    .iter()
                    .zip(cache.tiles())
                    .filter(|(old, tile)| !Arc::ptr_eq(old, &tile.image))
                    .count();
                assert_eq!(retired.len(), changed);
                assert!(changed < 4);
                for image in &retired {
                    assert!(!cache
                        .tiles()
                        .iter()
                        .any(|tile| Arc::ptr_eq(image, &tile.image)));
                }
                for (old, tile) in before.iter().zip(cache.tiles()) {
                    assert_eq!(
                        retired.iter().any(|image| Arc::ptr_eq(old, image)),
                        !Arc::ptr_eq(old, &tile.image)
                    );
                }
                assert_corners_match_fresh_render(&cache, plan, &source, &scene);
            }
        }
    }

    #[test]
    fn actual_corner_cache_redraws_changed_sampled_layer_and_keeps_frame_on_scene_failure() {
        let raw = raw();
        let source = source(&raw);
        let plan = plan();
        let mut pen = Annotation::new(Tool::Pen, Point { x: 11., y: 11. }, Style::default());
        pen.update(Point { x: 15., y: 13. });
        let mut blur = Annotation::new(Tool::Blur, Point { x: 1., y: 1. }, Style::default());
        blur.update(Point { x: 28., y: 23. });
        let scene = Arc::new(Scene::prepare(&[pen.clone(), blur.clone()], &source));
        let mut cache = CornerCache::default();
        cache
            .prepare_frame(plan, (7, &source), scene.clone(), source.canvas, true, &[])
            .unwrap();
        let old: Vec<_> = cache
            .tiles()
            .iter()
            .map(|tile| tile.image.clone())
            .collect();
        pen.update(Point { x: 18., y: 15. });
        let next =
            Arc::new(scene.updated_with_draft(&[pen.clone(), blur.clone()], &source, Some(0)));
        assert_eq!(scene.annotations[1], next.annotations[1]);
        let (
            crate::scene::Layer::Raster { image: before, .. },
            crate::scene::Layer::Raster { image: after, .. },
        ) = (&scene.layers[1], &next.layers[1])
        else {
            panic!("下层变化应生成新的模糊采样层");
        };
        assert!(!Arc::ptr_eq(before, after));
        let retired = cache
            .prepare_frame(plan, (7, &source), next.clone(), source.canvas, true, &[])
            .unwrap();
        assert_eq!(retired.len(), 4);
        for (old, tile) in old.iter().zip(cache.tiles()) {
            assert!(!Arc::ptr_eq(old, &tile.image));
            assert!(retired.iter().any(|image| Arc::ptr_eq(old, image)));
        }
        assert_corners_match_fresh_render(&cache, plan, &source, &next);
        let valid: Vec<_> = cache
            .tiles()
            .iter()
            .map(|tile| tile.image.clone())
            .collect();
        let body = cache.body_tiles()[0].image.clone();
        let mut failed = next.updated_with_draft(&[pen, blur], &source, Some(0));
        failed.errors.push("本次候选场景失败".into());
        assert!(cache
            .prepare_frame(
                plan,
                (7, &source),
                Arc::new(failed),
                source.canvas,
                true,
                &[]
            )
            .is_err());
        assert!(Arc::ptr_eq(&cache.cached_frame(7).unwrap().scene, &next));
        assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
        for (before, tile) in valid.iter().zip(cache.tiles()) {
            assert!(Arc::ptr_eq(before, &tile.image));
        }
        assert_eq!(cache.clear().len(), 5);
    }

    #[test]
    fn actual_corner_cache_retires_old_images_on_visibility_source_scene_mask_and_geometry_changes()
    {
        let raw = raw();
        let source = source(&raw);
        let mut mark = Annotation::new(
            Tool::Rectangle,
            Point { x: 0.5, y: 0.5 },
            Style {
                filled: true,
                color: 0x00ff00,
                ..Default::default()
            },
        );
        mark.update(Point { x: 6.5, y: 6.5 });
        let scene = Arc::new(Scene::prepare(&[mark], &source));
        let mut cache = CornerCache::default();
        cache
            .prepare(plan(), 7, scene.clone(), &source, None, true)
            .unwrap();
        let before: Vec<_> = cache
            .tiles()
            .iter()
            .map(|tile| tile.image.as_bytes(0).unwrap().to_vec())
            .collect();
        assert_eq!(
            cache
                .prepare(plan(), 7, scene.clone(), &source, None, false)
                .unwrap()
                .len(),
            4
        );
        let after: Vec<_> = cache
            .tiles()
            .iter()
            .map(|tile| tile.image.as_bytes(0).unwrap().to_vec())
            .collect();
        assert_ne!(before, after);
        assert_eq!(
            cache
                .prepare(plan(), 8, scene.clone(), &source, None, false)
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            cache
                .prepare(plan(), 8, Arc::new(Scene::default()), &source, None, false)
                .unwrap()
                .len(),
            4
        );
        let mask = Polygon::new(&[
            Point { x: 2., y: 2. },
            Point { x: 29., y: 2. },
            Point { x: 15., y: 23. },
        ])
        .unwrap();
        assert_eq!(
            cache
                .prepare(plan(), 8, scene.clone(), &source, Some(&mask), false)
                .unwrap()
                .len(),
            4
        );
        let changed = PresentationPreviewPlan::new(
            Rect::full(31, 25),
            plan().style(),
            PhysicalPoint { x: 3.5, y: 5.75 },
            1.375,
        )
        .unwrap();
        assert_eq!(
            cache
                .prepare(changed, 8, scene, &source, Some(&mask), false)
                .unwrap()
                .len(),
            4
        );
        assert_eq!(cache.clear().len(), 4);
        assert!(cache.tiles().is_empty());
        assert!(cache.clear().is_empty());
    }

    #[test]
    fn actual_corner_cache_failed_rebuild_keeps_last_valid_images_for_host_retirement() {
        let raw = raw();
        let source = source(&raw);
        let scene = Arc::new(Scene::default());
        let mut cache = CornerCache::default();
        cache
            .prepare(plan(), 7, scene.clone(), &source, None, true)
            .unwrap();
        let before: Vec<_> = cache
            .tiles()
            .iter()
            .map(|tile| tile.image.clone())
            .collect();
        let invalid = SourcePixels {
            canvas: Rect::full(1, 1),
            fill: None,
            planes: Vec::new(),
            overlay: None,
        };
        assert!(cache
            .prepare(plan(), 8, scene, &invalid, None, true)
            .is_err());
        for (before, tile) in before.iter().zip(cache.tiles()) {
            assert!(Arc::ptr_eq(before, &tile.image));
        }
        assert_eq!(cache.clear().len(), before.len());
    }

    #[test]
    fn actual_body_cache_keeps_gradient_alpha_and_reuses_pixels_across_scene_changes() {
        let mut raw = raw();
        for y in 0..raw.height {
            for x in 0..raw.width {
                let at = (y as usize * raw.width as usize + x as usize) * 4;
                raw.bgra[at..at + 4].copy_from_slice(&[
                    (x * 7 + y * 13) as u8,
                    (x * 11 + y * 17) as u8,
                    (x * 19 + y * 23) as u8,
                    [0, 97, 128, 255][(x + y) as usize % 4],
                ]);
            }
        }
        let before = raw.bgra.clone();
        let source = source(&raw);
        let plan = plan();
        let visible = Rect {
            x: 7,
            y: 5,
            width: 19,
            height: 14,
        };
        let mut cache = CornerCache::default();
        assert!(cache
            .prepare_body(plan, 7, &source, visible)
            .unwrap()
            .is_empty());
        assert_eq!(cache.body_tiles().len(), 1);
        let tile = &cache.body_tiles()[0];
        assert_eq!(
            tile.region,
            PresentationRegion {
                left: 12,
                top: 12,
                width: 27,
                height: 20,
            }
        );
        let expected = plan
            .render_content_region(&source, &Scene::default(), None, tile.region)
            .unwrap();
        assert_eq!(tile.image.as_bytes(0).unwrap(), expected.bgra);
        assert!(expected
            .bgra
            .chunks_exact(4)
            .any(|p| p[3] > 0 && p[3] < 255));
        let body = tile.image.clone();
        cache
            .prepare(plan, 7, Arc::new(Scene::default()), &source, None, true)
            .unwrap();
        let mut mark = Annotation::new(
            Tool::Rectangle,
            Point { x: 0.5, y: 0.5 },
            Style {
                filled: true,
                color: 0x00ff00,
                ..Default::default()
            },
        );
        mark.update(Point { x: 6.5, y: 6.5 });
        assert_eq!(
            cache
                .prepare(
                    plan,
                    7,
                    Arc::new(Scene::prepare(&[mark], &source)),
                    &source,
                    None,
                    true
                )
                .unwrap()
                .len(),
            4
        );
        assert!(cache
            .prepare_body(plan, 7, &source, visible)
            .unwrap()
            .is_empty());
        assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
        assert!(cache
            .prepare_body(
                plan,
                7,
                &source,
                Rect {
                    x: 31,
                    width: 1,
                    ..visible
                }
            )
            .is_err());
        assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
        assert_eq!(
            cache
                .prepare_body(
                    plan,
                    7,
                    &source,
                    Rect {
                        width: 18,
                        ..visible
                    }
                )
                .unwrap()
                .len(),
            1
        );
        assert_eq!(cache.clear().len(), 5);
        assert!(cache.body_tiles().is_empty() && cache.tiles().is_empty());
        assert_eq!(raw.bgra, before);

        // 透明红与 opaque 蓝之间的目标中心只混合有效预乘蓝，不能生成 straight 紫色。
        let transparent_edge = Image {
            width: 2,
            height: 1,
            bgra: vec![0, 0, 255, 0, 255, 0, 0, 255],
            readback_ms: 0.,
        };
        let edge_source = self::source(&transparent_edge);
        let edge_plan = PresentationPreviewPlan::new(
            edge_source.canvas,
            SelectionFinishStyle::default(),
            PhysicalPoint { x: 0.5, y: 0. },
            1.,
        )
        .unwrap();
        cache
            .prepare_body(edge_plan, 8, &edge_source, edge_source.canvas)
            .unwrap();
        assert_eq!(cache.body_tiles().len(), 1);
        let bytes = cache.body_tiles()[0].image.as_bytes(0).unwrap();
        assert_eq!(&bytes[4..8], &[255, 0, 0, 128]);
        assert_ne!(&bytes[4..8], &[128, 0, 128, 128]);
        assert_eq!(transparent_edge.bgra, [0, 0, 255, 0, 255, 0, 0, 255]);
    }

    #[test]
    fn dual_4k_preview_preflight_keeps_body_48m_and_export_32m_budgets_with_vector_corners() {
        // 仅规划几何，不分配双4K图像或运行GPUI。
        let placement = Placement {
            x: 0,
            y: 0,
            width: 8000,
            height: 8000,
        };
        let desktop = Placement {
            width: 7680,
            height: 4320,
            ..placement
        };
        let style = SelectionFinishStyle::default();
        let viewport = crate::pin_presentation_host::content_viewport(
            (4000, 4000),
            (placement.width, placement.height),
            Some(style),
        );
        assert_eq!(viewport.zoom, 2.);
        let visible =
            preview_visible_source_region(Rect::full(4000, 4000), viewport, placement, desktop)
                .unwrap();
        assert_eq!(visible, Rect::full(3842, 2162));
        let plan = PresentationPreviewPlan::new(
            Rect::full(4000, 4000),
            style,
            PhysicalPoint { x: 0., y: 0. },
            2.,
        )
        .unwrap();
        let regions = body_tile_regions(plan, visible).unwrap();
        assert_eq!(regions.len(), 4);
        assert!(regions.iter().all(|r| r.width <= 4096 && r.height <= 4096));
        assert_eq!(regions[0].left, 0);
        assert_eq!(regions[0].top, 0);
        assert_eq!(regions[1].left, 4096);
        assert_eq!(regions[2].top, 4096);
        assert_eq!(regions[3].width, 3588);
        assert_eq!(regions[3].height, 228);
        assert_eq!(
            regions
                .iter()
                .map(|r| u64::from(r.width) * u64::from(r.height))
                .sum::<u64>(),
            33_225_616
        );
        validate_preview_geometry((4000, 4000), style, placement, desktop).unwrap();
        assert!(super::super::PresentationOutputPlan::new(visible, style, 2.).is_err());
        assert!(validate_preview_geometry((4000, 4000), style, placement, placement).is_err());

        // 旧CPU角缓存拒绝4200px单边；矢量圆角呈现组只分配35.12M主体。
        let placement = Placement {
            width: 14_000,
            height: 14_000,
            ..placement
        };
        let style = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 60.,
            ..style
        };
        let plan = PresentationPreviewPlan::new(
            Rect::full(200, 200),
            style,
            PhysicalPoint { x: 0., y: 0. },
            70.,
        )
        .unwrap();
        let visible = preview_visible_source_region(
            plan.content,
            Viewport {
                zoom: 70.,
                offset: Point { x: 0., y: 0. },
            },
            placement,
            desktop,
        )
        .unwrap();
        assert_eq!(visible, Rect::full(112, 64));
        assert_eq!(
            body_tile_regions(plan, visible)
                .unwrap()
                .iter()
                .map(|r| u64::from(r.width) * u64::from(r.height))
                .sum::<u64>(),
            35_123_200
        );
        assert!(plan
            .corner_regions()
            .iter()
            .all(|r| r.width == 4200 && r.height == 4200));
        assert!(corner_tile_regions(plan).is_err());
        validate_preview_geometry((200, 200), style, placement, desktop).unwrap();

        // 旧CPU模型仍核主体与四角的总额；矢量呈现组不分配31.36M四角位图。
        let style = SelectionFinishStyle {
            corner_radius: 40.,
            ..style
        };
        let plan = PresentationPreviewPlan::new(
            Rect::full(200, 200),
            style,
            PhysicalPoint { x: 0., y: 0. },
            70.,
        )
        .unwrap();
        assert_eq!(
            corner_tile_regions(plan)
                .unwrap()
                .iter()
                .map(|r| u64::from(r.width) * u64::from(r.height))
                .sum::<u64>(),
            31_360_000
        );
        assert!(validate_preview_frame(plan, visible).is_err());
        validate_preview_geometry((200, 200), style, placement, desktop).unwrap();

        // Reset原图本身合法32M，但改变旋转后的长边方向会扩大可见目标分配。
        let desktop = Placement {
            x: 0,
            y: 0,
            width: 16_000,
            height: 4000,
        };
        let rotated = Placement {
            width: 5120,
            height: 16_000,
            ..desktop
        };
        let reset = Placement {
            width: 16_000,
            height: 5120,
            ..desktop
        };
        let style = SelectionFinishStyle::default();
        validate_preview_geometry((3200, 10_000), style, rotated, desktop).unwrap();
        assert!(validate_preview_geometry((10_000, 3200), style, reset, desktop).is_err());

        let desktop = Placement {
            width: 10_000,
            height: 6000,
            ..desktop
        };
        let rotated = Placement {
            width: 6000,
            height: 16_000,
            ..desktop
        };
        let reset = Placement {
            width: 16_000,
            height: 6000,
            ..desktop
        };
        validate_preview_geometry((3000, 8000), style, rotated, desktop).unwrap();
        assert!(validate_preview_geometry((8000, 3000), style, reset, desktop).is_err());
    }

    #[test]
    fn topology_change_fits_client_without_changing_source_contract_or_export_budget() {
        let content = (4000, 4000);
        let style = SelectionFinishStyle::default();
        let placement = Placement {
            x: 0,
            y: 0,
            width: 8000,
            height: 8000,
        };
        let before_desktop = Placement {
            width: 7680,
            height: 4320,
            ..placement
        };
        let after_desktop = placement;
        validate_preview_geometry(content, style, placement, before_desktop).unwrap();
        assert_eq!(
            fit_preview_placement(content, style, placement, before_desktop).unwrap(),
            placement
        );
        assert!(validate_preview_geometry(content, style, placement, after_desktop).is_err());
        let before_viewport = crate::pin_presentation_host::content_viewport(
            content,
            (placement.width, placement.height),
            Some(style),
        );
        let plan = PresentationPreviewPlan::new(
            Rect::full(content.0, content.1),
            style,
            PhysicalPoint { x: 0., y: 0. },
            f64::from(before_viewport.zoom),
        )
        .unwrap();
        let before_visible =
            preview_visible_source_region(plan.content, before_viewport, placement, before_desktop)
                .unwrap();
        let after_visible =
            preview_visible_source_region(plan.content, before_viewport, placement, after_desktop)
                .unwrap();
        assert_eq!(before_visible, Rect::full(3842, 2162));
        assert_eq!(after_visible, Rect::full(4000, 4000));
        assert_ne!(
            before_visible.intersection(after_visible),
            Some(after_visible)
        );

        let fitted = fit_preview_placement(content, style, placement, after_desktop).unwrap();
        assert_eq!((fitted.x, fitted.y), (placement.x, placement.y));
        assert!(fitted.width > 0 && fitted.width < placement.width);
        assert_eq!(fitted.width, fitted.height);
        validate_preview_geometry(content, style, fitted, after_desktop).unwrap();
        assert_eq!(
            fit_preview_placement(content, style, fitted, after_desktop).unwrap(),
            fitted
        );
        let fitted_viewport = crate::pin_presentation_host::content_viewport(
            content,
            (fitted.width, fitted.height),
            Some(style),
        );
        let fitted_plan = PresentationPreviewPlan::new(
            plan.content,
            style,
            PhysicalPoint {
                x: f64::from(fitted_viewport.offset.x),
                y: f64::from(fitted_viewport.offset.y),
            },
            f64::from(fitted_viewport.zoom),
        )
        .unwrap();
        assert_eq!(fitted_plan.content, plan.content);
        assert_eq!(fitted_plan.style(), plan.style());
        assert_eq!(fitted_plan.target.sx, fitted_plan.target.sy);
        assert_eq!(
            fitted_plan.content_bounds().width,
            fitted_plan.content_bounds().height
        );
        let source_point = Point {
            x: 173.25,
            y: 2819.75,
        };
        let restored = fitted_viewport.image_point(fitted_viewport.window_point(source_point));
        assert!((restored.x - source_point.x).abs() < 0.001);
        assert!((restored.y - source_point.y).abs() < 0.001);
        assert_eq!(
            super::super::PresentationOutputPlan::new(plan.content, style, 1.)
                .unwrap()
                .scene_projection()
                .source,
            plan.content
        );
        assert!(super::super::PresentationOutputPlan::new(plan.content, style, 2.).is_err());

        // 非方形窗口保留有符号位置；客户区只在最后舍入，内容始终等比contain。
        let placement = Placement {
            x: -1920,
            y: -160,
            width: 12_000,
            height: 8000,
        };
        let fitted = fit_preview_placement(content, style, placement, placement).unwrap();
        assert_eq!((fitted.x, fitted.y), (placement.x, placement.y));
        assert!(fitted.width < placement.width && fitted.height < placement.height);
        let cross_width = u64::from(fitted.width) * u64::from(placement.height);
        let cross_height = u64::from(fitted.height) * u64::from(placement.width);
        assert!(cross_width.abs_diff(cross_height) <= u64::from(placement.width));
        validate_preview_geometry(content, style, fitted, placement).unwrap();
        assert!(fit_preview_placement((4000, 9000), style, placement, placement).is_err());
        assert!(fit_preview_placement(
            content,
            style,
            Placement {
                width: 0,
                ..placement
            },
            placement,
        )
        .is_err());
        assert!(fit_preview_placement(
            content,
            style,
            Placement {
                x: i32::MAX - 5,
                ..placement
            },
            placement,
        )
        .is_err());

        // 外部改变的客户区即使只在小桌面露出少量源像素，也必须符合宿主16k两轴上限。
        for (width, height) in [(20_000, 100), (100, 20_000)] {
            let placement = Placement {
                width,
                height,
                ..placement
            };
            let desktop = Placement {
                width: 100,
                height: 100,
                ..placement
            };
            let viewport = crate::pin_presentation_host::content_viewport(
                (100, 100),
                (width, height),
                Some(style),
            );
            let plan = PresentationPreviewPlan::new(
                Rect::full(100, 100),
                style,
                PhysicalPoint {
                    x: f64::from(viewport.offset.x),
                    y: f64::from(viewport.offset.y),
                },
                f64::from(viewport.zoom),
            )
            .unwrap();
            let visible =
                preview_visible_source_region(plan.content, viewport, placement, desktop).unwrap();
            validate_preview_frame(plan, visible).unwrap();
            assert!(validate_preview_geometry((100, 100), style, placement, desktop).is_err());
            let fitted = fit_preview_placement((100, 100), style, placement, desktop).unwrap();
            assert_eq!((fitted.x, fitted.y), (placement.x, placement.y));
            assert!(fitted.width <= crate::pin_model::MAX_PIN_WINDOW_SIDE);
            assert!(fitted.height <= crate::pin_model::MAX_PIN_WINDOW_SIDE);
            validate_preview_geometry((100, 100), style, fitted, desktop).unwrap();
        }
        let placement = Placement {
            width: 500,
            height: 500,
            ..placement
        };
        let desktop = Placement {
            width: 20_000,
            height: 20_000,
            ..placement
        };
        validate_preview_geometry((100, 100), style, placement, desktop).unwrap();
    }

    #[test]
    fn actual_preview_frame_failure_keeps_matching_body_corners_scene_and_retirement() {
        let raw = raw();
        let source = source(&raw);
        let plan = plan();
        let mut blur = Annotation::new(Tool::Blur, Point { x: 2., y: 2. }, Style::default());
        blur.update(Point { x: 18., y: 17. });
        let scene = Arc::new(Scene::prepare(&[blur], &source));
        let crate::scene::Layer::Raster { image: patch, .. } = &scene.layers[0] else {
            panic!("模糊场景应生成实际raster映射");
        };
        let patch_pixels =
            image::RgbaImage::from_raw(patch.width, patch.height, patch.bgra.clone()).unwrap();
        let patch_preview = Arc::new(RenderImage::new(vec![image::Frame::new(patch_pixels)]));
        let rasters = vec![(patch.clone(), patch_preview.clone())];
        let mut cache = CornerCache::default();
        cache
            .prepare_frame(
                plan,
                (7, &source),
                scene.clone(),
                source.canvas,
                true,
                &rasters,
            )
            .unwrap();
        let body: Vec<_> = cache.body_tiles().iter().map(|t| t.image.clone()).collect();
        let corners: Vec<_> = cache.tiles().iter().map(|t| t.image.clone()).collect();
        let frame = cache.cached_frame(7).unwrap();
        assert_eq!(frame.plan, plan);
        assert!(Arc::ptr_eq(&frame.scene, &scene));
        assert!(frame.annotations_visible);
        assert_eq!(frame.visible, source.canvas);
        assert!(Arc::ptr_eq(&frame.rasters[0].0, patch));
        assert!(Arc::ptr_eq(&frame.rasters[0].1, &patch_preview));
        assert!(cache.cached_frame(8).is_none());

        // 候选body借用的中心源ROI完整有效，候选四角没有源像素而失败。
        // 这会在新body已构建后失败，不能发布半帧或遗失旧纹理的退休引用。
        let center_pixels = [91_u8, 83, 71, 128].repeat(8 * 8);
        let center_source = SourcePixels {
            canvas: source.canvas,
            fill: None,
            overlay: None,
            planes: vec![PixelPlane {
                rect: Rect {
                    x: 10,
                    y: 9,
                    width: 8,
                    height: 8,
                },
                bgra: &center_pixels,
            }],
        };
        let visible = Rect {
            x: 12,
            y: 11,
            width: 2,
            height: 2,
        };
        let candidate_body = body_tile_regions(plan, visible).unwrap();
        for region in candidate_body {
            plan.render_content_region(&center_source, &Scene::default(), None, region)
                .unwrap();
        }
        assert!(cache
            .prepare_frame(
                plan,
                (8, &center_source),
                Arc::new(Scene::default()),
                visible,
                true,
                &[],
            )
            .is_err());
        assert_eq!(cache.body_tiles().len(), body.len());
        assert_eq!(cache.tiles().len(), corners.len());
        for (before, after) in body.iter().zip(cache.body_tiles()) {
            assert!(Arc::ptr_eq(before, &after.image));
        }
        for (before, after) in corners.iter().zip(cache.tiles()) {
            assert!(Arc::ptr_eq(before, &after.image));
        }
        let fallback = cache.cached_frame(7).unwrap();
        assert_eq!(fallback.plan, plan);
        assert!(Arc::ptr_eq(&fallback.scene, &scene));
        assert!(fallback.annotations_visible);
        assert_eq!(fallback.visible, source.canvas);
        assert_eq!(fallback.rasters.len(), 1);
        assert!(Arc::ptr_eq(&fallback.rasters[0].0, patch));
        assert!(Arc::ptr_eq(&fallback.rasters[0].1, &patch_preview));
        assert!(Arc::ptr_eq(&cache.raster_previews()[0].1, &patch_preview));
        assert!(cache.cached_frame(8).is_none());

        let oversized = PresentationPreviewPlan::new(
            source.canvas,
            plan.style(),
            PhysicalPoint { x: 3.25, y: 5.75 },
            300.,
        )
        .unwrap();
        assert!(cache
            .prepare_frame(
                oversized,
                (7, &source),
                scene.clone(),
                source.canvas,
                true,
                &[]
            )
            .is_err());
        assert_eq!(cache.cached_frame(7).unwrap().plan, plan);
        for (before, after) in body.iter().zip(cache.body_tiles()) {
            assert!(Arc::ptr_eq(before, &after.image));
        }

        // 相同几何恢复有效完整源时，只替换已变化的主体，返回旧body供宿主退休。
        let retired = cache
            .prepare_frame(plan, (7, &source), scene.clone(), visible, true, &rasters)
            .unwrap();
        assert_eq!(retired.len(), body.len());
        for (before, retired) in body.iter().zip(&retired) {
            assert!(Arc::ptr_eq(before, retired));
        }
        for (before, after) in corners.iter().zip(cache.tiles()) {
            assert!(Arc::ptr_eq(before, &after.image));
        }
        assert_eq!(cache.cached_frame(7).unwrap().visible, visible);
        // 来源与plan仍匹配也不能把旧中心ROI当成新的全内容可见范围。
        let retained = cache.cached_frame(7).unwrap();
        assert_eq!(retained.plan, plan);
        assert_ne!(
            retained.visible.intersection(source.canvas),
            Some(source.canvas)
        );
        let retired = cache.clear();
        assert_eq!(retired.len(), 6);
        assert!(retired
            .iter()
            .any(|image| Arc::ptr_eq(image, &patch_preview)));
        assert!(cache.cached_frame(7).is_none());
        assert!(raw.bgra.chunks_exact(4).all(|p| p == [23, 45, 67, 97]));
    }
}

#[cfg(test)]
mod content_group_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        scene::PixelPlane,
    };

    fn raw() -> Image {
        Image {
            width: 31,
            height: 25,
            bgra: [23, 45, 67, 97].repeat(31 * 25),
            readback_ms: 0.,
        }
    }
    fn source(raw: &Image) -> SourcePixels<'_> {
        let canvas = Rect::full(raw.width, raw.height);
        SourcePixels {
            canvas,
            fill: None,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &raw.bgra,
            }],
            overlay: None,
        }
    }
    fn plan(content: Rect) -> PresentationPreviewPlan {
        PresentationPreviewPlan::new(
            content,
            SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 6.25,
                ..Default::default()
            },
            super::super::PhysicalPoint { x: 3.25, y: 5.75 },
            1.25,
        )
        .unwrap()
    }

    #[test]
    fn actual_content_group_frame_keeps_raw_alpha_and_has_no_cpu_corner_or_scene_tiles() {
        let raw = raw();
        let source = source(&raw);
        let mut rectangle = Annotation::new(
            Tool::Rectangle,
            Point { x: 0., y: 0. },
            Style {
                filled: true,
                color: 0xff0000,
                ..Default::default()
            },
        );
        rectangle.update(Point { x: 12., y: 12. });
        let scene = Arc::new(Scene::prepare(&[rectangle], &source));
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame(
                plan(source.canvas),
                (7, &source),
                scene.clone(),
                source.canvas,
                true,
                &[],
            )
            .unwrap();
        assert!(cache.tiles.is_empty());
        assert_eq!(cache.body_tiles().len(), 1);
        assert!(cache.body_tiles()[0]
            .image
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .all(|pixel| pixel == [23, 45, 67, 97]));
        assert!(Arc::ptr_eq(&cache.cached_frame(7).unwrap().scene, &scene));
        assert!(raw
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel == [23, 45, 67, 97]));
    }

    #[test]
    fn actual_content_group_scene_changes_reuse_background_and_bind_latest_visibility() {
        let raw = raw();
        let source = source(&raw);
        let plan = plan(source.canvas);
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame(
                plan,
                (7, &source),
                Arc::new(Scene::default()),
                source.canvas,
                true,
                &[],
            )
            .unwrap();
        let body = cache.body_tiles()[0].image.clone();
        let mut pen = Annotation::new(Tool::Pen, Point { x: 0., y: 0. }, Style::default());
        pen.update(Point { x: 13., y: 10. });
        let scene = Arc::new(Scene::prepare(&[pen], &source));
        assert!(cache
            .prepare_group_frame(plan, (7, &source), scene.clone(), source.canvas, false, &[])
            .unwrap()
            .is_empty());
        assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
        let frame = cache.cached_frame(7).unwrap();
        assert!(Arc::ptr_eq(&frame.scene, &scene));
        assert!(!frame.annotations_visible);
        assert!(cache.tiles.is_empty());
    }

    #[test]
    fn actual_content_group_failed_candidate_keeps_whole_last_frame_and_can_retry() {
        let raw = raw();
        let source = source(&raw);
        let plan = plan(source.canvas);
        let previous = Arc::new(Scene::default());
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame(
                plan,
                (7, &source),
                previous.clone(),
                source.canvas,
                true,
                &[],
            )
            .unwrap();
        let body = cache.body_tiles()[0].image.clone();
        let mut invalid = Scene::default();
        invalid.errors.push("当前Scene无法完整绘制".into());
        assert!(cache
            .prepare_group_frame(
                plan,
                (8, &source),
                Arc::new(invalid),
                source.canvas,
                false,
                &[]
            )
            .is_err());
        let frame = cache.cached_frame(7).unwrap();
        assert!(Arc::ptr_eq(&frame.scene, &previous));
        assert!(frame.annotations_visible);
        assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
        assert!(cache.cached_frame(8).is_none());
        let oversized = PresentationPreviewPlan::new(
            source.canvas,
            plan.style(),
            super::super::PhysicalPoint { x: 3.25, y: 5.75 },
            300.,
        )
        .unwrap();
        assert!(cache
            .prepare_group_frame(
                oversized,
                (8, &source),
                previous.clone(),
                source.canvas,
                true,
                &[]
            )
            .is_err());
        assert!(Arc::ptr_eq(&body, &cache.body_tiles()[0].image));
        let retired = cache
            .prepare_group_frame(
                plan,
                (8, &source),
                previous.clone(),
                source.canvas,
                true,
                &[],
            )
            .unwrap();
        assert_eq!(retired.len(), 1);
        assert!(Arc::ptr_eq(&retired[0], &body));
        assert!(cache.cached_frame(7).is_none());
        assert!(Arc::ptr_eq(
            &cache.cached_frame(8).unwrap().scene,
            &previous
        ));
        assert_eq!(cache.clear().len(), 1);
        assert!(cache.cached_frame(8).is_none());
    }

    #[test]
    fn actual_content_group_nonzero_source_rect_preserves_absolute_sampling_phase() {
        let mut raw = raw();
        for y in 0..raw.height {
            for x in 0..raw.width {
                let at = (y as usize * raw.width as usize + x as usize) * 4;
                raw.bgra[at..at + 4].copy_from_slice(&[
                    x as u8 * 5,
                    y as u8 * 6,
                    71,
                    if x % 2 == 0 { 0 } else { 200 },
                ]);
            }
        }
        let original = raw.bgra.clone();
        let source = source(&raw);
        let content = Rect {
            x: 4,
            y: 5,
            width: 22,
            height: 18,
        };
        let plan = plan(content);
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame(
                plan,
                (7, &source),
                Arc::new(Scene::default()),
                content,
                true,
                &[],
            )
            .unwrap();
        let tile = &cache.body_tiles()[0];
        let bytes = tile.image.as_bytes(0).unwrap();
        for y in 0..tile.region.height {
            for x in 0..tile.region.width {
                let point = Point {
                    x: content.x as f32 + (tile.region.left as f32 + x as f32 + 0.5 - 3.25) / 1.25,
                    y: content.y as f32 + (tile.region.top as f32 + y as f32 + 0.5 - 5.75) / 1.25,
                };
                let expected = crate::pin_color_sample::linear_pixel(&source, point).unwrap();
                let at = (y as usize * tile.region.width as usize + x as usize) * 4;
                assert_eq!(&bytes[at..at + 4], expected);
            }
        }
        assert_eq!(raw.bgra, original);
        assert!(cache.tiles.is_empty());
        assert_eq!(cache.cached_frame(7).unwrap().visible, content);
    }

    #[test]
    fn staged_body_is_adopted_only_by_matching_frame_and_retired_otherwise() {
        let mut raw = raw();
        for (index, pixel) in raw.bgra.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[(index * 7) as u8, (index * 3) as u8, 90, (index * 11) as u8]);
        }
        let pixels = source(&raw);
        let canvas = pixels.canvas;
        let at = |zoom: f64| {
            PresentationPreviewPlan::new(
                canvas,
                plan(canvas).style(),
                super::super::PhysicalPoint { x: 3.25, y: 5.75 },
                zoom,
            )
            .unwrap()
        };
        let (before, after) = (at(1.25), at(1.5));
        let scene = Arc::new(Scene::default());
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame(before, (7, &pixels), scene.clone(), canvas, true, &[])
            .unwrap();
        let old = cache.body_tiles()[0].image.clone();
        // 预备不动已发布帧；同键重复预备不重采。
        assert!(cache
            .stage_body(after, 7, &pixels, canvas)
            .unwrap()
            .is_empty());
        assert!(Arc::ptr_eq(&old, &cache.body_tiles()[0].image));
        assert!(cache.group_frame_matches(before, (7, &pixels), &scene, canvas, true, None));
        let staged = cache.staged_body.as_ref().unwrap().tiles[0].image.clone();
        assert!(cache
            .stage_body(after, 7, &pixels, canvas)
            .unwrap()
            .is_empty());
        assert!(Arc::ptr_eq(
            &staged,
            &cache.staged_body.as_ref().unwrap().tiles[0].image
        ));
        // 改尺寸后的同键帧直接采用，退休旧底图；像素与现采逐字节相同。
        let retired = cache
            .prepare_group_frame(after, (7, &pixels), scene.clone(), canvas, true, &[])
            .unwrap();
        assert!(retired.iter().any(|image| Arc::ptr_eq(image, &old)));
        assert!(Arc::ptr_eq(&staged, &cache.body_tiles()[0].image));
        assert!(cache.staged_body.is_none());
        assert!(cache.group_frame_matches(after, (7, &pixels), &scene, canvas, true, None));
        let mut fresh = CornerCache::default();
        fresh
            .prepare_group_frame(after, (7, &pixels), scene.clone(), canvas, true, &[])
            .unwrap();
        assert_eq!(fresh.body_tiles()[0].region, cache.body_tiles()[0].region);
        assert_eq!(
            fresh.body_tiles()[0].image.as_bytes(0),
            cache.body_tiles()[0].image.as_bytes(0)
        );
        // 当前帧已是目标键时不预备。
        assert!(cache
            .stage_body(after, 7, &pixels, canvas)
            .unwrap()
            .is_empty());
        assert!(cache.staged_body.is_none());
        // 实际几何与预备不符（如系统改写定位）：预备退休，本帧照常现采。
        cache.stage_body(before, 7, &pixels, canvas).unwrap();
        let unused = cache.staged_body.as_ref().unwrap().tiles[0].image.clone();
        let retired = cache
            .prepare_group_frame(at(1.75), (7, &pixels), scene.clone(), canvas, true, &[])
            .unwrap();
        assert!(retired.iter().any(|image| Arc::ptr_eq(image, &unused)));
        assert!(!Arc::ptr_eq(&unused, &cache.body_tiles()[0].image));
        assert!(cache.staged_body.is_none());
        // 原图身份改变时同键预备也不能采用。
        cache.stage_body(after, 7, &pixels, canvas).unwrap();
        let stale = cache.staged_body.as_ref().unwrap().tiles[0].image.clone();
        let copy = Image {
            width: raw.width,
            height: raw.height,
            bgra: raw.bgra.clone(),
            readback_ms: 0.,
        };
        let other = source(&copy);
        let retired = cache
            .prepare_group_frame(after, (7, &other), scene.clone(), canvas, true, &[])
            .unwrap();
        assert!(retired.iter().any(|image| Arc::ptr_eq(image, &stale)));
        assert!(!Arc::ptr_eq(&stale, &cache.body_tiles()[0].image));
        // 失败的帧准备不消耗预备；clear 连同预备一起退休。
        cache.stage_body(at(2.), 7, &other, canvas).unwrap();
        assert!(cache
            .prepare_group_frame(at(300.), (7, &other), scene.clone(), canvas, true, &[])
            .is_err());
        assert!(cache.staged_body.is_some());
        assert_eq!(cache.clear().len(), 2);
        assert!(cache.staged_body.is_none());
    }
}

#[cfg(test)]
mod polygon_content_group_tests {
    use super::*;
    use crate::{annotation::Point, scene::PixelPlane};

    fn plan(content: Rect) -> PresentationPreviewPlan {
        PresentationPreviewPlan::new(
            content,
            SelectionFinishStyle::default(),
            super::super::PhysicalPoint { x: 2.25, y: 4.5 },
            1.25,
        )
        .unwrap()
    }
    fn polygon() -> Polygon {
        Polygon::new(&[
            Point { x: 4., y: 5. },
            Point { x: 28., y: 5. },
            Point { x: 28., y: 12. },
            Point { x: 12., y: 12. },
            Point { x: 12., y: 23. },
            Point { x: 4., y: 23. },
        ])
        .unwrap()
    }
    fn pixels<'a>(a: &'a [u8], b: &'a [u8], overlay: Option<&'a [u8]>) -> SourcePixels<'a> {
        SourcePixels {
            canvas: Rect::full(32, 24),
            fill: None,
            planes: vec![
                PixelPlane {
                    rect: Rect::full(16, 24),
                    bgra: a,
                },
                PixelPlane {
                    rect: Rect {
                        x: 16,
                        y: 0,
                        width: 16,
                        height: 24,
                    },
                    bgra: b,
                },
            ],
            overlay: overlay.map(|bgra| PixelPlane {
                rect: Rect {
                    x: 8,
                    y: 7,
                    width: 2,
                    height: 2,
                },
                bgra,
            }),
        }
    }

    #[test]
    fn actual_polygon_group_cache_binds_every_plane_cursor_plan_scene_visibility_and_mask() {
        let a = [20, 30, 40, 200].repeat(16 * 24);
        let b = [50, 60, 70, 150].repeat(16 * 24);
        let replacement = [90, 80, 70, 150].repeat(16 * 24);
        let cursor = [230, 240, 250, 255].repeat(4);
        let source = pixels(&a, &b, None);
        let current = plan(source.canvas);
        let mask = polygon();
        let scene = Arc::new(Scene::default());
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame_masked(
                current,
                (7, &source),
                scene.clone(),
                source.canvas,
                true,
                Some(&mask),
                &[],
            )
            .unwrap();
        let matches =
            |candidate: &SourcePixels<'_>, plan, scene: &Arc<Scene>, visible, shown, mask| {
                cache.group_frame_matches(plan, (7, candidate), scene, visible, shown, mask)
            };
        assert!(matches(
            &source,
            current,
            &scene,
            source.canvas,
            true,
            Some(&mask)
        ));
        assert!(!matches(
            &pixels(&a, &replacement, None),
            current,
            &scene,
            source.canvas,
            true,
            Some(&mask)
        ));
        assert!(!matches(
            &pixels(&a, &b, Some(&cursor)),
            current,
            &scene,
            source.canvas,
            true,
            Some(&mask)
        ));
        assert!(!matches(
            &source,
            current,
            &Arc::new(Scene::default()),
            source.canvas,
            true,
            Some(&mask)
        ));
        assert!(!matches(
            &source,
            current,
            &scene,
            source.canvas,
            false,
            Some(&mask)
        ));
        assert!(!matches(
            &source,
            current,
            &scene,
            Rect::full(31, 24),
            true,
            Some(&mask)
        ));
        assert!(!matches(
            &source,
            current,
            &scene,
            source.canvas,
            true,
            None
        ));
        let moved = PresentationPreviewPlan::new(
            source.canvas,
            current.style(),
            super::super::PhysicalPoint { x: 2.5, y: 4.5 },
            1.25,
        )
        .unwrap();
        assert!(!matches(
            &source,
            moved,
            &scene,
            source.canvas,
            true,
            Some(&mask)
        ));
        let old = cache.body_tiles()[0].image.clone();
        cache
            .prepare_group_frame_masked(
                current,
                (7, &pixels(&a, &replacement, None)),
                scene.clone(),
                source.canvas,
                true,
                Some(&mask),
                &[],
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&old, &cache.body_tiles()[0].image));
        assert!(!cache.group_frame_matches(
            current,
            (7, &source),
            &scene,
            source.canvas,
            true,
            Some(&mask)
        ));
    }

    #[test]
    fn actual_polygon_group_uses_nonzero_mesh_and_signed_fractional_source_projection() {
        let mask = polygon();
        let viewport = Viewport {
            zoom: 1.25,
            offset: Point { x: -3.5, y: -7.75 },
        };
        assert!(polygon_content_path(&mask, viewport, 1.5).is_ok());
        let vertices = polygon_content_triangles(&mask, viewport, 1.5)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(vertices.len(), mask.triangles.len() * 3);
        for ((actual, uv), source) in vertices.iter().zip(mask.triangles.iter().flatten()) {
            assert!((f32::from(actual.x) - (source.x * 1.25 - 3.5) / 1.5).abs() < 0.0001);
            assert!((f32::from(actual.y) - (source.y * 1.25 - 7.75) / 1.5).abs() < 0.0001);
            assert_eq!(*uv, point(0., 1.));
        }
        // 凹槽处没有triangle；first-point fan会错误覆盖这个位置。
        let probe = Point { x: 20., y: 18. };
        assert!(!mask.triangles.iter().any(|t| {
            let cross =
                |a: Point, b: Point| (b.x - a.x) * (probe.y - a.y) - (b.y - a.y) * (probe.x - a.x);
            let signs = [cross(t[0], t[1]), cross(t[1], t[2]), cross(t[2], t[0])];
            signs.iter().all(|v| *v >= 0.) || signs.iter().all(|v| *v <= 0.)
        }));
        assert!(polygon_content_path(&mask, viewport, 0.).is_err());
        let twice = mask
            .points
            .iter()
            .copied()
            .chain(mask.points.iter().copied())
            .collect::<Vec<_>>();
        let double_winding = Polygon::new(&twice).unwrap();
        assert!(polygon_content_path(&double_winding, viewport, 1.5).is_ok());
        let twice_vertices = polygon_content_triangles(&double_winding, viewport, 1.5)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .count();
        let area = |mask: &Polygon| {
            mask.triangles
                .iter()
                .map(|[a, b, c]| ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() / 2.)
                .sum::<f32>()
        };
        assert!((area(&mask) - area(&double_winding)).abs() < 0.01);
        assert_eq!(twice_vertices, double_winding.triangles.len() * 3);
    }

    #[test]
    fn actual_polygon_group_failed_mask_candidate_keeps_complete_raw_frame_without_baking() {
        let a = [20, 30, 40, 200].repeat(16 * 24);
        let b = [50, 60, 70, 150].repeat(16 * 24);
        let source = pixels(&a, &b, None);
        let current = plan(source.canvas);
        let mask = polygon();
        let scene = Arc::new(Scene::default());
        let mut cache = CornerCache::default();
        cache
            .prepare_group_frame_masked(
                current,
                (7, &source),
                scene.clone(),
                source.canvas,
                true,
                Some(&mask),
                &[],
            )
            .unwrap();
        let old = cache.body_tiles()[0].image.clone();
        assert!(old
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .all(|pixel| pixel[3] != 0));
        assert!(cache.tiles.is_empty());
        let invalid = Polygon {
            points: mask.points.clone(),
            triangles: Arc::from([]),
            bounds: mask.bounds,
        };
        assert!(cache
            .prepare_group_frame_masked(
                current,
                (7, &source),
                scene.clone(),
                source.canvas,
                false,
                Some(&invalid),
                &[]
            )
            .is_err());
        assert!(Arc::ptr_eq(&old, &cache.body_tiles()[0].image));
        assert!(cache
            .cached_group_frame(current, (7, &source), source.canvas, Some(&mask))
            .is_some());
        assert!(cache
            .cached_group_frame(current, (7, &source), source.canvas, Some(&invalid))
            .is_none());
        assert!(!cache.group_frame_matches(
            current,
            (7, &source),
            &scene,
            source.canvas,
            false,
            Some(&invalid)
        ));
        assert!(a.chunks_exact(4).all(|p| p == [20, 30, 40, 200]));
        assert!(b.chunks_exact(4).all(|p| p == [50, 60, 70, 150]));
        cache.clear();
        assert!(!cache.group_frame_matches(
            current,
            (7, &source),
            &scene,
            source.canvas,
            true,
            Some(&mask)
        ));
    }
}
