//! 复合马赛克/模糊：离散区域采同一对象下层，稀疏输出块互不重叠。
use crate::{
    annotation::{Annotation, Point, Tool},
    capture::Image,
    geometry::Rect,
    scene::{Scene, SourcePixels},
    Result,
};
use std::{collections::BTreeMap, sync::Arc};

const TILE_SIDE: u32 = 256;
const MAX_EFFECT_PIXELS: u64 = 32_000_000;
const MAX_REGION_VISITS: u64 = 128_000_000;

#[derive(Clone)]
pub struct EffectPatch {
    pub rect: Rect,
    pub image: Arc<Image>,
}
struct OutputTile {
    rect: Rect,
    regions: Vec<usize>,
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

fn clipped_bounds(bounds: crate::annotation::Bounds, canvas: Rect) -> Option<Rect> {
    let x = bounds.x.floor().max(canvas.x as f32);
    let y = bounds.y.floor().max(canvas.y as f32);
    let right = (bounds.x + bounds.width)
        .ceil()
        .min((canvas.x + canvas.width) as f32);
    let bottom = (bounds.y + bounds.height)
        .ceil()
        .min((canvas.y + canvas.height) as f32);
    (right > x && bottom > y).then_some(Rect {
        x: x as u32,
        y: y as u32,
        width: (right - x) as u32,
        height: (bottom - y) as u32,
    })
}

/// 输出按全画布固定tile划分，每个像素只属于一个patch；远隔区域不分配间隙。
fn output_tiles(a: &Annotation, canvas: Rect) -> Result<Vec<OutputTile>> {
    let mut tiles = BTreeMap::<(u32, u32), OutputTile>::new();
    let mut visits = 0_u64;
    for (index, bounds) in a.effect_regions_world().into_iter().enumerate() {
        let Some(rect) = clipped_bounds(bounds, canvas) else {
            continue;
        };
        visits += u64::from(rect.width) * u64::from(rect.height);
        if visits > MAX_REGION_VISITS {
            return Err("复合效果区域覆盖工作量过大".into());
        }
        let right = (rect.x + rect.width - 1) / TILE_SIDE;
        let bottom = (rect.y + rect.height - 1) / TILE_SIDE;
        for ty in rect.y / TILE_SIDE..=bottom {
            for tx in rect.x / TILE_SIDE..=right {
                let tile = Rect {
                    x: tx * TILE_SIDE,
                    y: ty * TILE_SIDE,
                    width: TILE_SIDE.min(canvas.x + canvas.width - tx * TILE_SIDE),
                    height: TILE_SIDE.min(canvas.y + canvas.height - ty * TILE_SIDE),
                };
                let part = rect.intersection(tile).ok_or("复合效果分块没有交集")?;
                let tile = tiles.entry((tx, ty)).or_insert_with(|| OutputTile {
                    rect: part,
                    regions: Vec::new(),
                });
                tile.rect = union(tile.rect, part);
                tile.regions.push(index);
                if tiles.len() > 65_536 {
                    return Err("复合效果分块数量过大".into());
                }
            }
        }
    }
    let pixels: u64 = tiles
        .values()
        .map(|t| u64::from(t.rect.width) * u64::from(t.rect.height))
        .sum();
    if pixels > MAX_EFFECT_PIXELS {
        return Err("复合效果超过3200万输出像素预算".into());
    }
    Ok(tiles.into_values().collect())
}

fn sampling_rect(rect: Rect, canvas: Rect, block: u32, blur: bool) -> Rect {
    let halo = if blur { (block / 3).max(1) * 3 } else { 0 };
    let (left, top, right, bottom) = if blur {
        (
            rect.x.saturating_sub(halo).max(canvas.x),
            rect.y.saturating_sub(halo).max(canvas.y),
            (rect.x + rect.width + halo).min(canvas.x + canvas.width),
            (rect.y + rect.height + halo).min(canvas.y + canvas.height),
        )
    } else {
        (
            (rect.x / block * block).max(canvas.x),
            (rect.y / block * block).max(canvas.y),
            ((rect.x + rect.width).div_ceil(block) * block).min(canvas.x + canvas.width),
            ((rect.y + rect.height).div_ceil(block) * block).min(canvas.y + canvas.height),
        )
    };
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

pub(crate) fn prepare(
    a: &Annotation,
    source: &SourcePixels<'_>,
    below: &Scene,
) -> Result<Arc<[EffectPatch]>> {
    if a.effect_regions.is_none() || !a.valid() || !matches!(a.tool, Tool::Mosaic | Tool::Blur) {
        return Err("复合效果几何或参数无效".into());
    }
    if let Some(error) = below.first_error() {
        return Err(error.into());
    }
    let regions = &a.effect_regions.as_ref().ok_or("复合效果没有区域")?.regions;
    let inverse = a.rotation_transform().inverse();
    let block = a.style.block_size.clamp(2, 64);
    let alpha = (a.style.opacity * 255.).round() as u8;
    let blur = a.tool == Tool::Blur;
    let mut patches = Vec::new();
    let world = a.effect_regions_world();
    for tile in output_tiles(a, source.canvas)? {
        let rect = tile.rect;
        let read_rect = sampling_rect(rect, source.canvas, block, blur);
        let mut pixels = source.read(read_rect);
        // 每块都只采相同below；不把前一patch加入场景后再采下一块。
        below.try_apply(&mut pixels, read_rect)?;
        if blur {
            for _ in 0..3 {
                crate::raster::box_blur(&mut pixels, (block / 3).max(1));
            }
        }
        let mut image = crate::raster::empty(rect);
        let mut mask = vec![false; rect.width as usize * rect.height as usize];
        for index in tile.regions {
            let Some(part) = clipped_bounds(world[index], rect) else {
                continue;
            };
            for gy in part.y..part.y + part.height {
                for gx in part.x..part.x + part.width {
                    let offset = ((gy - rect.y) * rect.width + gx - rect.x) as usize;
                    if !mask[offset] {
                        let local = inverse.map(Point {
                            x: gx as f32 + 0.5,
                            y: gy as f32 + 0.5,
                        });
                        mask[offset] = regions[index].contains(local, 0.);
                    }
                }
            }
        }
        let mut cells = std::collections::HashMap::<(u32, u32), [u8; 3]>::new();
        for y in 0..rect.height {
            for x in 0..rect.width {
                let gx = rect.x + x;
                let gy = rect.y + y;
                if !mask[(y * rect.width + x) as usize] {
                    continue;
                }
                let dst = ((y * rect.width + x) as usize) * 4;
                if blur {
                    let src =
                        (((gy - read_rect.y) * read_rect.width + gx - read_rect.x) as usize) * 4;
                    image.bgra[dst..dst + 3].copy_from_slice(&pixels.bgra[src..src + 3]);
                } else {
                    let cx = gx / block * block;
                    let cy = gy / block * block;
                    let color = cells.entry((cx, cy)).or_insert_with(|| {
                        let cell = Rect {
                            x: cx,
                            y: cy,
                            width: block,
                            height: block,
                        }
                        .intersection(source.canvas)
                        .expect("covered effect cell belongs to canvas");
                        let mut sum = [0_u64; 3];
                        for sy in cell.y..cell.y + cell.height {
                            for sx in cell.x..cell.x + cell.width {
                                let src = (((sy - read_rect.y) * read_rect.width + sx - read_rect.x)
                                    as usize)
                                    * 4;
                                for (channel, total) in sum.iter_mut().enumerate() {
                                    *total += u64::from(pixels.bgra[src + channel]);
                                }
                            }
                        }
                        let n = u64::from(cell.width) * u64::from(cell.height);
                        [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
                    });
                    image.bgra[dst..dst + 3].copy_from_slice(color);
                }
                image.bgra[dst + 3] = alpha;
            }
        }
        patches.push(EffectPatch {
            rect,
            image: Arc::new(image),
        });
    }
    Ok(patches.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Bounds, Style},
        scene::{Layer, PixelPlane},
    };

    fn pattern(canvas: Rect) -> Vec<u8> {
        let mut bytes = Vec::new();
        for y in 0..canvas.height {
            for x in 0..canvas.width {
                bytes.extend_from_slice(&[
                    (x * 7 + y * 11) as u8,
                    (x * 13 + y * 3) as u8,
                    (x * 5 + y * 17) as u8,
                    255,
                ]);
            }
        }
        bytes
    }
    fn make_source(canvas: Rect, bytes: &[u8]) -> SourcePixels<'_> {
        SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: bytes,
            }],
            overlay: None,
        }
    }
    fn make_effect(tool: Tool, regions: Vec<Bounds>, block: u32, opacity: f32) -> Annotation {
        let mut a = Annotation::new(
            tool,
            Point { x: 0., y: 0. },
            Style {
                block_size: block,
                opacity,
                ..Style::default()
            },
        );
        a.set_effect_regions(regions, true).unwrap();
        a
    }
    fn lower() -> Annotation {
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 250., y: 0. },
            Style {
                color: 0xd9317e,
                filled: true,
                ..Style::default()
            },
        );
        a.update(Point { x: 265., y: 64. });
        a
    }
    fn clone_image(image: &Image) -> Image {
        Image {
            width: image.width,
            height: image.height,
            bgra: image.bgra.clone(),
            readback_ms: 0.,
        }
    }

    #[test]
    fn overlapping_blur_regions_use_one_lower_baseline_and_union_alpha_once() {
        let canvas = Rect::full(160, 64);
        let bytes = pattern(canvas);
        let source = make_source(canvas, &bytes);
        let first = Bounds {
            x: 20.,
            y: 20.,
            width: 60.,
            height: 20.,
        };
        let second = Bounds {
            x: 60.,
            y: 20.,
            width: 60.,
            height: 20.,
        };
        let composite = make_effect(Tool::Blur, vec![first, second, first], 21, 0.45);
        let joined = make_effect(
            Tool::Blur,
            vec![Bounds {
                x: 20.,
                y: 20.,
                width: 100.,
                height: 20.,
            }],
            21,
            0.45,
        );
        let scene = Scene::prepare(&[composite], &source);
        let reference = Scene::prepare(&[joined], &source);
        assert!(scene.first_error().is_none());
        assert_eq!(scene.layers.len(), 1);
        let mut actual = source.read(canvas);
        let mut expected = source.read(canvas);
        scene.try_apply(&mut actual, canvas).unwrap();
        reference.try_apply(&mut expected, canvas).unwrap();
        assert_eq!(actual.bgra, expected.bgra);
        assert_ne!(actual.bgra, bytes);
    }

    #[test]
    fn sparse_blur_tiles_match_full_three_pass_halo_and_keep_uncovered_gap() {
        let canvas = Rect::full(600, 64);
        let bytes = pattern(canvas);
        let source = make_source(canvas, &bytes);
        let regions = vec![
            Bounds {
                x: 245.,
                y: 15.,
                width: 30.,
                height: 20.,
            },
            Bounds {
                x: 535.,
                y: 15.,
                width: 25.,
                height: 20.,
            },
        ];
        let effect = make_effect(Tool::Blur, regions, 21, 0.65);
        let below = Scene::prepare(&[lower()], &source);
        let patches = prepare(&effect, &source, &below).unwrap();
        assert_eq!(patches.len(), 3);
        for (i, a) in patches.iter().enumerate() {
            for b in patches.iter().skip(i + 1) {
                assert!(a.rect.intersection(b.rect).is_none());
            }
        }
        let mut baseline = source.read(canvas);
        below.try_apply(&mut baseline, canvas).unwrap();
        let mut filtered = clone_image(&baseline);
        for _ in 0..3 {
            crate::raster::box_blur(&mut filtered, 7);
        }
        let mut expected = clone_image(&baseline);
        let alpha = (0.65_f32 * 255.).round() / 255.;
        for y in 0..canvas.height {
            for x in 0..canvas.width {
                if effect.hit(
                    Point {
                        x: x as f32 + 0.5,
                        y: y as f32 + 0.5,
                    },
                    0.,
                    true,
                ) {
                    let i = ((y * canvas.width + x) * 4) as usize;
                    crate::raster::over_pixel(
                        &mut expected.bgra[i..i + 4],
                        &filtered.bgra[i..i + 3],
                        alpha,
                    );
                }
            }
        }
        let mut actual = clone_image(&baseline);
        for patch in patches.iter() {
            crate::raster::composite(&mut actual, canvas, &patch.image, patch.rect);
        }
        assert_eq!(actual.bgra, expected.bgra);
        let gap = ((20 * canvas.width + 400) * 4) as usize;
        assert_eq!(&actual.bgra[gap..gap + 4], &baseline.bgra[gap..gap + 4]);
    }

    #[test]
    fn sparse_mosaic_keeps_canvas_grid_across_tiles_and_samples_lower_objects() {
        let canvas = Rect::full(600, 64);
        let bytes = pattern(canvas);
        let source = make_source(canvas, &bytes);
        let effect = make_effect(
            Tool::Mosaic,
            vec![
                Bounds {
                    x: -5.,
                    y: 0.,
                    width: 24.,
                    height: 15.,
                },
                Bounds {
                    x: 245.,
                    y: 15.,
                    width: 35.,
                    height: 20.,
                },
                Bounds {
                    x: 535.,
                    y: 15.,
                    width: 25.,
                    height: 20.,
                },
            ],
            17,
            1.,
        );
        let below = Scene::prepare(&[lower()], &source);
        let mut baseline = source.read(canvas);
        below.try_apply(&mut baseline, canvas).unwrap();
        let patches = prepare(&effect, &source, &below).unwrap();
        let mut actual = clone_image(&baseline);
        for patch in patches.iter() {
            crate::raster::composite(&mut actual, canvas, &patch.image, patch.rect);
        }
        for y in 0..canvas.height {
            for x in 0..canvas.width {
                let i = ((y * canvas.width + x) * 4) as usize;
                if !effect.hit(
                    Point {
                        x: x as f32 + 0.5,
                        y: y as f32 + 0.5,
                    },
                    0.,
                    true,
                ) {
                    assert_eq!(&actual.bgra[i..i + 4], &baseline.bgra[i..i + 4]);
                    continue;
                }
                let cell = Rect {
                    x: x / 17 * 17,
                    y: y / 17 * 17,
                    width: 17,
                    height: 17,
                }
                .intersection(canvas)
                .unwrap();
                let mut sum = [0_u64; 3];
                for cy in cell.y..cell.y + cell.height {
                    for cx in cell.x..cell.x + cell.width {
                        let offset = ((cy * canvas.width + cx) * 4) as usize;
                        for (channel, total) in sum.iter_mut().enumerate() {
                            *total += u64::from(baseline.bgra[offset + channel]);
                        }
                    }
                }
                let count = u64::from(cell.width) * u64::from(cell.height);
                assert_eq!(
                    &actual.bgra[i..i + 3],
                    &[
                        (sum[0] / count) as u8,
                        (sum[1] / count) as u8,
                        (sum[2] / count) as u8
                    ]
                );
            }
        }
        let crop = Rect {
            x: 253,
            y: 17,
            width: 15,
            height: 10,
        };
        let scene = Scene::prepare(&[lower(), effect], &source);
        let mut part = source.read(crop);
        scene.try_apply(&mut part, crop).unwrap();
        for y in 0..crop.height {
            for x in 0..crop.width {
                let p = ((y * crop.width + x) * 4) as usize;
                let f = (((y + crop.y) * canvas.width + x + crop.x) * 4) as usize;
                assert_eq!(&part.bgra[p..p + 4], &actual.bgra[f..f + 4]);
            }
        }
        // 旋转后的真实两块之间仍留空；不能拿整个旋转外接框覆盖像素。
        let mut rotated = make_effect(
            Tool::Mosaic,
            vec![
                Bounds {
                    x: 10.,
                    y: 10.,
                    width: 20.,
                    height: 10.,
                },
                Bounds {
                    x: 50.,
                    y: 10.,
                    width: 20.,
                    height: 10.,
                },
            ],
            17,
            1.,
        );
        rotated.rotate_about(Point { x: 40., y: 15. }, 90.);
        let rotated_scene = Scene::prepare(&[rotated], &source);
        let mut output = source.read(canvas);
        rotated_scene.try_apply(&mut output, canvas).unwrap();
        let covered = ((30 * canvas.width + 40) * 4) as usize;
        let gap = ((15 * canvas.width + 40) * 4) as usize;
        let outside = ((30 * canvas.width + 30) * 4) as usize;
        assert_ne!(
            &output.bgra[covered..covered + 4],
            &bytes[covered..covered + 4]
        );
        assert_eq!(&output.bgra[gap..gap + 4], &bytes[gap..gap + 4]);
        assert_eq!(
            &output.bgra[outside..outside + 4],
            &bytes[outside..outside + 4]
        );
    }

    #[test]
    fn composite_scene_prefix_reuses_one_layer_and_invalidates_on_lower_or_source_change() {
        let canvas = Rect::full(600, 64);
        let bytes = pattern(canvas);
        let source = make_source(canvas, &bytes);
        let effect = make_effect(
            Tool::Mosaic,
            vec![Bounds {
                x: 245.,
                y: 15.,
                width: 35.,
                height: 20.,
            }],
            17,
            1.,
        );
        let mut top = lower();
        top.translate(40., 0.);
        let mut annotations = vec![lower(), effect, top];
        let first = Scene::prepare(&annotations, &source);
        let Layer::EffectRegions { patches: old } = &first.layers[1] else {
            panic!("one effect layer expected")
        };
        annotations[2].translate(10., 0.);
        let upper = first.updated(&annotations, &source);
        let Layer::EffectRegions { patches: reused } = &upper.layers[1] else {
            panic!("effect layer expected")
        };
        assert!(Arc::ptr_eq(old, reused));
        assert_eq!(upper.layers.len(), annotations.len());
        annotations[0].style.color = 0x29ee44;
        let lower = upper.updated(&annotations, &source);
        let Layer::EffectRegions { patches: rebuilt } = &lower.layers[1] else {
            panic!("effect layer expected")
        };
        assert!(!Arc::ptr_eq(old, rebuilt));
        let mut other_bytes = bytes.clone();
        other_bytes[0] ^= 255;
        let other = make_source(canvas, &other_bytes);
        let replaced = lower.updated(&annotations, &other);
        let Layer::EffectRegions {
            patches: new_source,
        } = &replaced.layers[1]
        else {
            panic!("effect layer expected")
        };
        assert!(!Arc::ptr_eq(rebuilt, new_source));
        assert!(replaced.first_error().is_none());
        annotations[1].style.opacity = f32::NAN;
        let invalid = replaced.updated(&annotations, &other);
        assert!(invalid.first_error().is_some());
        let mut output = other.read(canvas);
        let original = output.bgra.clone();
        assert!(invalid.try_apply(&mut output, canvas).is_err());
        assert_eq!(output.bgra, original);
    }
}
