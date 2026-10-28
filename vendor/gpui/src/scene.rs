// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AtlasTextureId, AtlasTile, Background, Bounds, ContentMask, Corners, Edges, Hsla, Pixels,
    Point, Radians, ScaledPixels, Size, bounds_tree::BoundsTree, point,
};
use std::{
    fmt::Debug,
    iter::Peekable,
    ops::{Add, Range, Sub},
    slice,
};

#[allow(non_camel_case_types, unused)]
pub(crate) type PathVertex_ScaledPixels = PathVertex<ScaledPixels>;

pub(crate) type DrawOrder = u32;

#[cfg(windows)]
use std::sync::{Arc, atomic::AtomicU64};

#[derive(Default)]
pub(crate) struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<DrawOrder>,
    pub(crate) shadows: Vec<Shadow>,
    pub(crate) quads: Vec<Quad>,
    pub(crate) paths: Vec<Path<ScaledPixels>>,
    pub(crate) underlines: Vec<Underline>,
    pub(crate) monochrome_sprites: Vec<MonochromeSprite>,
    pub(crate) polychrome_sprites: Vec<PolychromeSprite>,
    pub(crate) surfaces: Vec<PaintSurface>,
    #[cfg(windows)]
    rounded_content_groups: usize,
    #[cfg(windows)]
    grouped_batches: Vec<GroupedBatch>,
}

impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.paths.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
        #[cfg(windows)]
        {
            self.rounded_content_groups = 0;
            self.grouped_batches.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    pub fn push_layer(&mut self, bounds: Bounds<ScaledPixels>) {
        let order = self.primitive_bounds.insert(bounds);
        self.layer_stack.push(order);
        self.paint_operations
            .push(PaintOperation::StartLayer(bounds));
    }

    pub fn pop_layer(&mut self) {
        self.layer_stack.pop();
        self.paint_operations.push(PaintOperation::EndLayer);
    }

    /// A compositing barrier, rather than a draw-order layer. Every child keeps
    /// its original paint order until the group is composited once by Windows.
    #[cfg(windows)]
    pub(crate) fn push_rounded_content_group(&mut self, group: RoundedContentGroup) -> usize {
        self.rounded_content_groups += 1;
        let index = self.paint_operations.len();
        self.paint_operations
            .push(PaintOperation::StartRoundedContentGroup(group));
        index
    }

    #[cfg(windows)]
    pub(crate) fn set_rounded_content_ready(&mut self, index: usize, ready: bool) -> bool {
        match self.paint_operations.get_mut(index) {
            Some(PaintOperation::StartRoundedContentGroup(group)) => {
                group.content_ready = ready;
                true
            }
            _ => false,
        }
    }

    /// 保留层内容未完整绘制时撤销本次键，renderer 只合成本帧、不登记可复用内容。
    #[cfg(windows)]
    pub(crate) fn clear_retained_key(&mut self, index: usize) {
        if let Some(PaintOperation::StartRoundedContentGroup(RoundedContentGroup {
            retained: Some(op),
            ..
        })) = self.paint_operations.get_mut(index)
        {
            op.key = 0;
        }
    }

    #[cfg(windows)]
    pub(crate) fn pop_rounded_content_group(&mut self) {
        // Even an unmatched end must reach the renderer's balance validation.
        self.rounded_content_groups += 1;
        self.paint_operations
            .push(PaintOperation::EndRoundedContentGroup);
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        let mut primitive = primitive.into();
        let clipped_bounds = primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds);

        if clipped_bounds.is_empty() {
            return;
        }

        let order = self
            .layer_stack
            .last()
            .copied()
            .unwrap_or_else(|| self.primitive_bounds.insert(clipped_bounds));
        match &mut primitive {
            Primitive::Shadow(shadow) => {
                shadow.order = order;
                self.shadows.push(shadow.clone());
            }
            Primitive::Quad(quad) => {
                quad.order = order;
                self.quads.push(quad.clone());
            }
            Primitive::Path(path) => {
                path.order = order;
                path.id = PathId(self.paths.len());
                self.paths.push(path.clone());
            }
            Primitive::Underline(underline) => {
                underline.order = order;
                self.underlines.push(underline.clone());
            }
            Primitive::MonochromeSprite(sprite) => {
                sprite.order = order;
                self.monochrome_sprites.push(sprite.clone());
            }
            Primitive::PolychromeSprite(sprite) => {
                sprite.order = order;
                self.polychrome_sprites.push(sprite.clone());
            }
            Primitive::Surface(surface) => {
                surface.order = order;
                self.surfaces.push(surface.clone());
            }
        }
        self.paint_operations
            .push(PaintOperation::Primitive(primitive));
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        for operation in &prev_scene.paint_operations[range] {
            match operation {
                PaintOperation::Primitive(primitive) => self.insert_primitive(primitive.clone()),
                PaintOperation::StartLayer(bounds) => self.push_layer(*bounds),
                PaintOperation::EndLayer => self.pop_layer(),
                #[cfg(windows)]
                PaintOperation::StartRoundedContentGroup(group) => {
                    self.push_rounded_content_group(group.clone());
                }
                #[cfg(windows)]
                PaintOperation::EndRoundedContentGroup => self.pop_rounded_content_group(),
            }
        }
    }

    /// Group boundaries are barriers. Adjacent compatible primitives may batch,
    /// but typed arrays retain insertion order and paths never need re-cloning.
    #[cfg(windows)]
    fn finish_grouped_batches(&mut self) {
        const MAX_BATCH: usize = 4096;
        self.grouped_batches.clear();
        let mut offsets = [0_usize; 7];
        for operation in &self.paint_operations {
            let (kind, texture, isolated) = match operation {
                PaintOperation::StartRoundedContentGroup(group) => {
                    self.grouped_batches
                        .push(GroupedBatch::Start(group.clone()));
                    continue;
                }
                PaintOperation::EndRoundedContentGroup => {
                    self.grouped_batches.push(GroupedBatch::End);
                    continue;
                }
                PaintOperation::StartLayer(_) | PaintOperation::EndLayer => continue,
                PaintOperation::Primitive(primitive) => match primitive {
                    Primitive::Shadow(_) => (PrimitiveKind::Shadow, None, false),
                    Primitive::Quad(_) => (PrimitiveKind::Quad, None, false),
                    Primitive::Path(path) => (
                        PrimitiveKind::Path,
                        None,
                        path.image.is_some() || path.coverage_union,
                    ),
                    Primitive::Underline(_) => (PrimitiveKind::Underline, None, false),
                    Primitive::MonochromeSprite(sprite) => (
                        PrimitiveKind::MonochromeSprite,
                        Some(sprite.tile.texture_id),
                        false,
                    ),
                    Primitive::PolychromeSprite(sprite) => (
                        PrimitiveKind::PolychromeSprite,
                        Some(sprite.tile.texture_id),
                        false,
                    ),
                    Primitive::Surface(_) => (PrimitiveKind::Surface, None, false),
                },
            };
            let index = offsets[kind as usize];
            offsets[kind as usize] += 1;
            if !isolated {
                if let Some(GroupedBatch::Primitives {
                    kind: previous,
                    range,
                    texture: previous_texture,
                    isolated: false,
                }) = self.grouped_batches.last_mut()
                {
                    if *previous == kind
                        && *previous_texture == texture
                        && range.end == index
                        && range.len() < MAX_BATCH
                    {
                        range.end += 1;
                        continue;
                    }
                }
            }
            self.grouped_batches.push(GroupedBatch::Primitives {
                kind,
                range: index..index + 1,
                texture,
                isolated,
            });
        }
    }

    pub fn finish(&mut self) {
        #[cfg(windows)]
        if self.rounded_content_groups > 0 {
            self.finish_grouped_batches();
            return;
        }
        self.shadows.sort_by_key(|shadow| shadow.order);
        self.quads.sort_by_key(|quad| quad.order);
        self.paths.sort_by_key(|path| path.order);
        self.underlines.sort_by_key(|underline| underline.order);
        self.monochrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.polychrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.surfaces.sort_by_key(|surface| surface.order);
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub(crate) fn batches(&self) -> impl Iterator<Item = PrimitiveBatch<'_>> {
        #[cfg(windows)]
        if self.rounded_content_groups > 0 {
            return SceneBatchIterator::Grouped {
                scene: self,
                iterator: self.grouped_batches.iter(),
            };
        }
        SceneBatchIterator::Sorted(BatchIterator {
            shadows: &self.shadows,
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads: &self.quads,
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths: &self.paths,
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines: &self.underlines,
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites: &self.monochrome_sprites,
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            polychrome_sprites: &self.polychrome_sprites,
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces: &self.surfaces,
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    PolychromeSprite,
    Surface,
}

pub(crate) enum PaintOperation {
    Primitive(Primitive),
    StartLayer(Bounds<ScaledPixels>),
    EndLayer,
    #[cfg(windows)]
    StartRoundedContentGroup(RoundedContentGroup),
    #[cfg(windows)]
    EndRoundedContentGroup,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
pub(crate) struct RoundedContentGroup {
    pub(crate) bounds: Bounds<ScaledPixels>,
    pub(crate) clip_bounds: Bounds<ScaledPixels>,
    pub(crate) corner_radii: Corners<ScaledPixels>,
    pub(crate) corner_radii_y: Corners<ScaledPixels>,
    pub(crate) mask: Option<Arc<Path<ScaledPixels>>>,
    pub(crate) completion: Option<Arc<AtomicU64>>,
    pub(crate) content_ready: bool,
    /// 跨帧保留的内容层；`None` 为普通逐帧圆角组。
    pub(crate) retained: Option<RetainedOp>,
}

/// 一次保留层绘制请求。`id` 每次 paint 唯一，同一 Scene 重复 Present 时据此直接复用。
#[cfg(windows)]
#[derive(Clone, Debug)]
pub(crate) struct RetainedOp {
    pub(crate) layer: Arc<crate::RetainedLayer>,
    pub(crate) id: u64,
    /// 本次完成后纹理内容的键；0 表示内容不完整，不能被后续帧复用。
    pub(crate) key: u64,
    pub(crate) mode: crate::RetainedPaint,
}

#[cfg(all(test, windows))]
mod rounded_content_group_tests {
    use super::*;

    fn bounds(x: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(2.)),
            crate::size(ScaledPixels(80.), ScaledPixels(60.)),
        )
    }
    fn group() -> RoundedContentGroup {
        RoundedContentGroup {
            bounds: bounds(1.25),
            clip_bounds: bounds(1.25),
            corner_radii: Corners::all(ScaledPixels(5.)),
            corner_radii_y: Corners::all(ScaledPixels(7.)),
            mask: None,
            completion: None,
            content_ready: true,
            retained: None,
        }
    }
    fn quad(x: f32) -> Quad {
        Quad {
            bounds: bounds(x),
            content_mask: ContentMask { bounds: bounds(0.) },
            ..Quad::default()
        }
    }
    fn image() -> PolychromeSprite {
        PolychromeSprite {
            order: 0,
            image_sampling: 1,
            grayscale: false,
            opacity: 0.5,
            bounds: bounds(3.),
            content_mask: ContentMask { bounds: bounds(0.) },
            corner_radii: Default::default(),
            tile: AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: crate::AtlasTextureKind::Polychrome,
                },
                tile_id: crate::TileId(1),
                padding: 0,
                bounds: Bounds::new(
                    point(crate::DevicePixels(0), crate::DevicePixels(0)),
                    crate::size(crate::DevicePixels(8), crate::DevicePixels(8)),
                ),
            },
        }
    }
    fn sequence(scene: &Scene) -> Vec<&'static str> {
        scene
            .batches()
            .map(|batch| match batch {
                PrimitiveBatch::StartRoundedContentGroup(_) => "begin",
                PrimitiveBatch::EndRoundedContentGroup => "end",
                PrimitiveBatch::Quads(values) => {
                    assert!(!values.is_empty());
                    "quad"
                }
                PrimitiveBatch::Paths(values) => {
                    assert_eq!(values.len(), 1);
                    "path"
                }
                PrimitiveBatch::PolychromeSprites { sprites, .. } => {
                    assert_eq!(sprites.len(), 1);
                    "image"
                }
                _ => panic!("夹具只绘制真实quad/path/image"),
            })
            .collect()
    }

    #[::core::prelude::v1::test]
    fn actual_scene_groups_preserve_mixed_image_path_order_and_path_identity() {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(0.));
        scene.push_rounded_content_group(group());
        scene.insert_primitive(image());
        let mut path = Path::new(point(Pixels(2.), Pixels(2.)));
        path.line_to(point(Pixels(10.), Pixels(2.)));
        path.line_to(point(Pixels(10.), Pixels(10.)));
        let mut path = path.scale(1.);
        path.content_mask = ContentMask { bounds: bounds(0.) };
        assert!(!path.vertices.is_empty());
        scene.insert_primitive(path);
        scene.insert_primitive(quad(4.));
        scene.pop_rounded_content_group();
        scene.insert_primitive(quad(5.));
        scene.finish();
        assert_eq!(
            sequence(&scene),
            ["quad", "begin", "image", "path", "quad", "end", "quad"]
        );
        let actual = scene
            .batches()
            .find_map(|batch| match batch {
                PrimitiveBatch::Paths(paths) => Some(paths[0].id),
                _ => None,
            })
            .unwrap();
        assert_eq!(actual.0, scene.paths[0].id.0);
    }

    #[::core::prelude::v1::test]
    fn actual_scene_nested_groups_and_sort_layers_replay_without_cross_group_batches() {
        let mut original = Scene::default();
        original.push_layer(bounds(0.));
        original.push_rounded_content_group(group());
        original.insert_primitive(quad(1.));
        original.push_rounded_content_group(group());
        original.insert_primitive(image());
        original.pop_rounded_content_group();
        original.insert_primitive(quad(2.));
        original.pop_rounded_content_group();
        original.pop_layer();
        original.finish();
        let mut target = Scene::default();
        target.replay(0..original.len(), &original);
        target.finish();
        assert_eq!(
            sequence(&target),
            ["begin", "quad", "begin", "image", "end", "quad", "end"]
        );
        assert_eq!(sequence(&target), sequence(&original));
        assert!(
            matches!(target.batches().next(), Some(PrimitiveBatch::StartRoundedContentGroup(g)) if g.corner_radii_y.top_left.0 == 7.)
        );
    }

    #[::core::prelude::v1::test]
    fn actual_scene_partial_child_replay_keeps_current_parent_group_and_clear_restores_batching() {
        let mut original = Scene::default();
        original.push_rounded_content_group(group());
        original.insert_primitive(quad(1.));
        original.insert_primitive(quad(2.));
        original.pop_rounded_content_group();
        let mut target = Scene::default();
        target.push_rounded_content_group(group());
        target.replay(1..3, &original);
        target.pop_rounded_content_group();
        target.finish();
        assert_eq!(sequence(&target), ["begin", "quad", "end"]);
        assert!(
            target
                .batches()
                .any(|batch| matches!(batch, PrimitiveBatch::Quads(quads) if quads.len() == 2))
        );
        target.clear();
        target.insert_primitive(quad(3.));
        target.insert_primitive(quad(4.));
        target.finish();
        assert!(
            target
                .batches()
                .all(|batch| matches!(batch, PrimitiveBatch::Quads(_)))
        );
        assert_eq!(target.len(), 2);
        assert_eq!(target.rounded_content_groups, 0);
    }

    #[::core::prelude::v1::test]
    fn actual_scene_unbalanced_end_is_not_dropped_by_sorted_batching() {
        let mut scene = Scene::default();
        scene.pop_rounded_content_group();
        scene.insert_primitive(quad(1.));
        scene.finish();
        assert_eq!(sequence(&scene), ["end", "quad"]);
    }

    #[::core::prelude::v1::test]
    fn actual_scene_adjacent_batches_are_bounded_and_do_not_cross_compositing_barriers() {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(1.));
        scene.insert_primitive(quad(2.));
        scene.push_rounded_content_group(group());
        for _ in 0..5000 {
            scene.insert_primitive(quad(3.));
        }
        scene.pop_rounded_content_group();
        scene.insert_primitive(quad(4.));
        scene.finish();
        let counts: Vec<_> = scene
            .batches()
            .map(|batch| match batch {
                PrimitiveBatch::Quads(quads) => quads.len(),
                PrimitiveBatch::StartRoundedContentGroup(_) => 0,
                PrimitiveBatch::EndRoundedContentGroup => usize::MAX,
                _ => panic!("只包含quad"),
            })
            .collect();
        assert_eq!(counts, [2, 0, 4096, 904, usize::MAX, 1]);
        assert_eq!(scene.quads.len(), 5003);
    }

    #[::core::prelude::v1::test]
    fn actual_scene_image_batches_keep_atlas_identity_and_cached_partial_replay() {
        let mut original = Scene::default();
        original.push_rounded_content_group(group());
        original.insert_primitive(image());
        original.insert_primitive(image());
        let mut other = image();
        other.tile.texture_id.index = 1;
        original.insert_primitive(other);
        original.pop_rounded_content_group();
        original.finish();
        let mut replayed = Scene::default();
        replayed.replay(0..original.len(), &original);
        replayed.finish();
        let images: Vec<_> = replayed
            .batches()
            .filter_map(|batch| match batch {
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => Some((texture_id.index, sprites.len())),
                _ => None,
            })
            .collect();
        assert_eq!(images, [(0, 2), (1, 1)]);
        assert_eq!(replayed.polychrome_sprites[2].tile.texture_id.index, 1);
    }
    #[::core::prelude::v1::test]
    fn actual_scene_polygon_mask_and_cpu_ready_snapshot_survive_nested_replay() {
        let mut path = Path::new(crate::point(crate::px(-3.75), crate::px(5.25)));
        path.line_to(crate::point(crate::px(20.5), crate::px(5.25)));
        path.line_to(crate::point(crate::px(20.5), crate::px(30.75)));
        let mask = Arc::new(path.scale(1.25));
        let completion = Arc::new(AtomicU64::new(0));
        let mut outer = group();
        outer.mask = Some(mask.clone());
        outer.completion = Some(completion.clone());
        outer.content_ready = false;
        let mut original = Scene::default();
        let outer_index = original.push_rounded_content_group(outer);
        original.insert_primitive(quad(1.));
        let child_index = original.push_rounded_content_group(group());
        original.insert_primitive(quad(2.));
        original.pop_rounded_content_group();
        assert!(original.set_rounded_content_ready(outer_index, true));
        assert!(original.set_rounded_content_ready(child_index, false));
        assert!(!original.set_rounded_content_ready(outer_index + 1, false));
        assert!(!original.set_rounded_content_ready(usize::MAX, false));
        original.pop_rounded_content_group();
        original.finish();
        let mut replayed = Scene::default();
        replayed.replay(0..original.len(), &original);
        replayed.finish();
        let groups: Vec<_> = replayed
            .batches()
            .filter_map(|batch| match batch {
                PrimitiveBatch::StartRoundedContentGroup(group) => Some(group),
                _ => None,
            })
            .collect();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].content_ready);
        assert!(!groups[1].content_ready);
        assert!(Arc::ptr_eq(groups[0].mask.as_ref().unwrap(), &mask));
        assert!(Arc::ptr_eq(
            groups[0].completion.as_ref().unwrap(),
            &completion
        ));
        assert!(groups[1].mask.is_none());
        assert_eq!(
            groups[0].mask.as_ref().unwrap().vertices[0].xy_position.x.0,
            -3.75 * 1.25
        );
        assert!(original.set_rounded_content_ready(outer_index, false));
        assert!(groups[0].content_ready);
        assert_eq!(completion.load(std::sync::atomic::Ordering::Acquire), 0);
    }
}

#[cfg(windows)]
enum GroupedBatch {
    Start(RoundedContentGroup),
    End,
    Primitives {
        kind: PrimitiveKind,
        range: Range<usize>,
        texture: Option<AtlasTextureId>,
        isolated: bool,
    },
}

enum SceneBatchIterator<'a> {
    Sorted(BatchIterator<'a>),
    #[cfg(windows)]
    Grouped {
        scene: &'a Scene,
        iterator: slice::Iter<'a, GroupedBatch>,
    },
}

impl<'a> Iterator for SceneBatchIterator<'a> {
    type Item = PrimitiveBatch<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Sorted(iterator) => iterator.next(),
            #[cfg(windows)]
            Self::Grouped { scene, iterator } => Some(match iterator.next()? {
                GroupedBatch::Start(group) => {
                    PrimitiveBatch::StartRoundedContentGroup(group.clone())
                }
                GroupedBatch::End => PrimitiveBatch::EndRoundedContentGroup,
                GroupedBatch::Primitives {
                    kind,
                    range,
                    texture,
                    ..
                } => {
                    let range = range.clone();
                    match kind {
                        PrimitiveKind::Shadow => PrimitiveBatch::Shadows(&scene.shadows[range]),
                        PrimitiveKind::Quad => PrimitiveBatch::Quads(&scene.quads[range]),
                        PrimitiveKind::Path => PrimitiveBatch::Paths(&scene.paths[range]),
                        PrimitiveKind::Underline => {
                            PrimitiveBatch::Underlines(&scene.underlines[range])
                        }
                        PrimitiveKind::MonochromeSprite => PrimitiveBatch::MonochromeSprites {
                            texture_id: texture.expect("sprite texture was recorded"),
                            sprites: &scene.monochrome_sprites[range],
                        },
                        PrimitiveKind::PolychromeSprite => PrimitiveBatch::PolychromeSprites {
                            texture_id: texture.expect("sprite texture was recorded"),
                            sprites: &scene.polychrome_sprites[range],
                        },
                        PrimitiveKind::Surface => PrimitiveBatch::Surfaces(&scene.surfaces[range]),
                    }
                }
            }),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Primitive {
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
}

impl Primitive {
    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
        }
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    shadows: &'a [Shadow],
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads: &'a [Quad],
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths: &'a [Path<ScaledPixels>],
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, Path<ScaledPixels>>>,
    underlines: &'a [Underline],
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites: &'a [MonochromeSprite],
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    polychrome_sprites: &'a [PolychromeSprite],
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces: &'a [PaintSurface],
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut orders_and_kinds = [
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (self.paths_iter.peek().map(|q| q.order), PrimitiveKind::Path),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let (batch_kind, max_order_and_kind) = if first.0.is_some() {
            (first.1, (second.0.unwrap_or(u32::MAX), second.1))
        } else {
            return None;
        };

        match batch_kind {
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(
                    &self.shadows[shadows_start..shadows_end],
                ))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(&self.quads[quads_start..quads_end]))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                let isolated_path = self.paths[paths_start].image.is_some()
                    || self.paths[paths_start].coverage_union;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| {
                        !isolated_path
                            && path.image.is_none()
                            && !path.coverage_union
                            && (path.order, batch_kind) < max_order_and_kind
                    })
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(&self.paths[paths_start..paths_end]))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(
                    &self.underlines[underlines_start..underlines_end],
                ))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites: &self.monochrome_sprites[sprites_start..sprites_end],
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = self.polychrome_sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites: &self.polychrome_sprites[sprites_start..sprites_end],
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(
                    &self.surfaces[surfaces_start..surfaces_end],
                ))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveBatch<'a> {
    #[cfg(windows)]
    StartRoundedContentGroup(RoundedContentGroup),
    #[cfg(windows)]
    EndRoundedContentGroup,
    Shadows(&'a [Shadow]),
    Quads(&'a [Quad]),
    Paths(&'a [Path<ScaledPixels>]),
    Underlines(&'a [Underline]),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        sprites: &'a [MonochromeSprite],
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        sprites: &'a [PolychromeSprite],
    },
    Surfaces(&'a [PaintSurface]),
}

#[derive(Default, Debug, Clone)]
#[repr(C)]
pub(crate) struct Quad {
    pub order: DrawOrder,
    pub border_style: BorderStyle,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_color: Hsla,
    pub corner_radii: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    pub wavy: u32,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of a border.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// A dashed border.
    Dashed = 1,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(mut self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct MonochromeSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct PolychromeSprite {
    pub order: DrawOrder,
    // 复用原有对齐字段，保持 GPU 结构步长：0 为线性，1 为 Windows 精确最近邻。
    pub image_sampling: u32,
    pub grayscale: bool,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub tile: AtlasTile,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(target_os = "macos")]
    pub image_buffer: core_video::pixel_buffer::CVPixelBuffer,
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PathId(pub(crate) usize);

/// A line made up of a series of vertices and control points.
#[derive(Clone, Debug)]
pub struct Path<P: Clone + Debug + Default + PartialEq> {
    pub(crate) id: PathId,
    pub(crate) order: DrawOrder,
    pub(crate) bounds: Bounds<P>,
    pub(crate) content_mask: ContentMask<P>,
    pub(crate) vertices: Vec<PathVertex<P>>,
    pub(crate) color: Background,
    /// 单条路径先合并三角形覆盖率，最终合成时仅应用一次颜色和透明度。
    pub(crate) coverage_union: bool,
    /// 已含解析 AA fringe 的路径按像素中心求覆盖，避免 MSAA 的质心偏移重复平滑。
    pub(crate) analytic_coverage: bool,
    pub(crate) image: Option<PathImage<P>>,
    start: Point<P>,
    current: Point<P>,
    contour_count: usize,
}

/// 图像仅提供画刷颜色；路径覆盖率仍由矢量三角形与解析边缘产生。
#[derive(Clone, Debug)]
pub(crate) struct PathImage<P: Clone + Debug + Default + PartialEq> {
    pub(crate) bounds: Bounds<P>,
    pub(crate) clip_bounds: Bounds<P>,
    pub(crate) tile: AtlasTile,
}

impl Path<Pixels> {
    /// Create a new path with the given starting point.
    pub fn new(start: Point<Pixels>) -> Self {
        Self {
            id: PathId(0),
            order: DrawOrder::default(),
            vertices: Vec::new(),
            start,
            current: start,
            bounds: Bounds {
                origin: start,
                size: Default::default(),
            },
            content_mask: Default::default(),
            color: Default::default(),
            coverage_union: false,
            analytic_coverage: false,
            image: None,
            contour_count: 0,
        }
    }

    /// Scale this path by the given factor.
    pub fn scale(&self, factor: f32) -> Path<ScaledPixels> {
        Path {
            id: self.id,
            order: self.order,
            bounds: self.bounds.scale(factor),
            content_mask: self.content_mask.scale(factor),
            vertices: self
                .vertices
                .iter()
                .map(|vertex| vertex.scale(factor))
                .collect(),
            start: self.start.map(|start| start.scale(factor)),
            current: self.current.scale(factor),
            contour_count: self.contour_count,
            color: self.color,
            coverage_union: self.coverage_union,
            analytic_coverage: self.analytic_coverage,
            image: self.image.as_ref().map(|image| PathImage {
                bounds: image.bounds.scale(factor),
                clip_bounds: image.clip_bounds.scale(factor),
                tile: image.tile.clone(),
            }),
        }
    }

    /// Move the start, current point to the given point.
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        self.start = to;
        self.current = to;
    }

    /// Draw a straight line from the current point to the given point.
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }
        self.current = to;
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }

        self.push_triangle(
            (self.current, ctrl, to),
            (point(0., 0.), point(0.5, 0.), point(1., 1.)),
        );
        self.current = to;
    }

    /// Push a triangle to the Path.
    pub fn push_triangle(
        &mut self,
        xy: (Point<Pixels>, Point<Pixels>, Point<Pixels>),
        st: (Point<f32>, Point<f32>, Point<f32>),
    ) {
        self.bounds = self
            .bounds
            .union(&Bounds {
                origin: xy.0,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.1,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.2,
                size: Default::default(),
            });

        self.vertices.push(PathVertex {
            xy_position: xy.0,
            st_position: st.0,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.1,
            st_position: st.1,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.2,
            st_position: st.2,
            content_mask: Default::default(),
        });
    }
}

impl<T> Path<T>
where
    T: Clone + Debug + Default + PartialEq + PartialOrd + Add<T, Output = T> + Sub<Output = T>,
{
    #[allow(unused)]
    pub(crate) fn clipped_bounds(&self) -> Bounds<T> {
        self.bounds.intersect(&self.content_mask.bounds)
    }
}

impl From<Path<ScaledPixels>> for Primitive {
    fn from(path: Path<ScaledPixels>) -> Self {
        Primitive::Path(path)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct PathVertex<P: Clone + Debug + Default + PartialEq> {
    pub(crate) xy_position: Point<P>,
    pub(crate) st_position: Point<f32>,
    pub(crate) content_mask: ContentMask<P>,
}

impl PathVertex<Pixels> {
    pub fn scale(&self, factor: f32) -> PathVertex<ScaledPixels> {
        PathVertex {
            xy_position: self.xy_position.scale(factor),
            st_position: self.st_position,
            content_mask: self.content_mask.scale(factor),
        }
    }
}
