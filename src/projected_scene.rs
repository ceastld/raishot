//! 放大用户导出：源缓存保持原单位，活矢量在目标像素空间重新计算覆盖率。
use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    mesh_coverage::MeshCoverage,
    output_projection::OutputProjection,
    scene::{HighlighterTile, Layer, Scene, SourcePixels},
    spotlight::SpotlightGeometry,
    vector_mesh::VectorMesh,
    vector_path::{Span, VectorPath},
    Result,
};

impl Scene {
    pub(crate) fn apply_projected(
        &self,
        target: &mut Image,
        source: &SourcePixels<'_>,
        projection: &OutputProjection,
    ) -> Result<()> {
        self.compose_projected(target, source, projection, true)
    }
    /// 当前显示内容快照始终在目标栅格重绘；导出1×的既有像素合同单独保留。
    pub(crate) fn apply_display(
        &self,
        target: &mut Image,
        source: &SourcePixels<'_>,
        projection: &OutputProjection,
    ) -> Result<()> {
        self.compose_projected(target, source, projection, false)
    }
    fn compose_projected(
        &self,
        target: &mut Image,
        source: &SourcePixels<'_>,
        projection: &OutputProjection,
        preserve_identity: bool,
    ) -> Result<()> {
        let bytes = (projection.width as usize)
            .checked_mul(projection.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("投影输出大小溢出")?;
        if (target.width, target.height) != (projection.width, projection.height)
            || target.bgra.len() != bytes
        {
            return Err("场景投影与输出像素尺寸不一致".into());
        }
        if preserve_identity && projection.identity() {
            return self.try_apply(target, projection.source);
        }
        if self.layers.len() != self.annotations.len() {
            return Err("场景层与原标注索引不一致，不能放大重绘".into());
        }
        if let Some(error) = self.first_error() {
            return Err(error.into());
        }
        for (layer, a) in self.layers.iter().zip(&self.annotations) {
            match layer {
                Layer::Empty => {}
                Layer::Watermark { annotation } => {
                    crate::projected_text::paint(annotation, target, projection)?;
                }
                Layer::Vector { mesh, style, .. } => {
                    paint_vector(target, mesh, *style, projection)?
                }
                Layer::Highlighter {
                    mesh, style, tiles, ..
                } => paint_highlighter(target, mesh, *style, tiles, projection)?,
                Layer::Stroke { segments, style } => {
                    let path = VectorPath {
                        spans: segments
                            .iter()
                            .map(|&(start, end)| Span::Line { start, end })
                            .collect(),
                        ..Default::default()
                    };
                    let mesh = VectorMesh::build(&path, *style, false)?;
                    paint_vector(target, &mesh, *style, projection)?;
                }
                Layer::Mosaic { blocks } => {
                    for block in blocks.iter() {
                        let Some(part) = projection.bounds(rect_bounds(block.rect), 0.) else {
                            continue;
                        };
                        let color = [
                            block.color as u8,
                            (block.color >> 8) as u8,
                            (block.color >> 16) as u8,
                            255,
                        ];
                        for y in part.y..part.y + part.height {
                            for x in part.x..part.x + part.width {
                                if inside(block.rect, projection.source_center(x, y)) {
                                    pixel_mut(target, x, y).copy_from_slice(&color);
                                }
                            }
                        }
                    }
                }
                Layer::Raster { rect, image } => {
                    if a.tool.text() || a.tool == Tool::Serial {
                        crate::projected_text::paint(a, target, projection)?;
                    } else {
                        paint_cached(target, *rect, image, projection);
                    }
                }
                Layer::LocalFill { patches } => {
                    for patch in patches.iter() {
                        paint_cached(target, patch.rect, &patch.image, projection);
                    }
                }
                Layer::EffectRegions { patches } => {
                    for patch in patches.iter() {
                        paint_cached(target, patch.rect, &patch.image, projection);
                    }
                }
                Layer::Spotlight { geometry } => geometry.apply_projected(target, projection)?,
                Layer::Restore { .. } => restore(target, a, source, projection)?,
            }
        }
        Ok(())
    }
}

fn inside(rect: Rect, p: Point) -> bool {
    p.x >= rect.x as f32
        && p.y >= rect.y as f32
        && p.x < (rect.x as f32 + rect.width as f32)
        && p.y < (rect.y as f32 + rect.height as f32)
}
fn rect_bounds(rect: Rect) -> Bounds {
    Bounds {
        x: rect.x as f32,
        y: rect.y as f32,
        width: rect.width as f32,
        height: rect.height as f32,
    }
}
fn pixel_mut(image: &mut Image, x: u32, y: u32) -> &mut [u8] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    &mut image.bgra[i..i + 4]
}
fn mesh_region(mesh: &VectorMesh, projection: &OutputProjection) -> Result<Option<Rect>> {
    if mesh.vertices.is_empty() || mesh.edges.is_empty() {
        return Ok(None);
    }
    let mut left = f32::INFINITY;
    let mut top = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    let mut bottom = f32::NEG_INFINITY;
    for &p in &mesh.vertices {
        if !p.x.is_finite() || !p.y.is_finite() {
            return Err("导出网格坐标不是有限数值".into());
        }
        left = left.min(p.x);
        top = top.min(p.y);
        right = right.max(p.x);
        bottom = bottom.max(p.y);
    }
    Ok(projection.bounds(
        Bounds {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        },
        1.,
    ))
}
fn paint_vector(
    target: &mut Image,
    mesh: &VectorMesh,
    style: Style,
    projection: &OutputProjection,
) -> Result<()> {
    // 外边界扫描需要并集网格；画布显示网格在这里首次按需并集。
    let mesh = mesh.exact()?;
    let Some(region) = mesh_region(mesh, projection)? else {
        return Ok(());
    };
    let coverage = MeshCoverage::projected(mesh, projection, region)?;
    let color = [
        style.color as u8,
        (style.color >> 8) as u8,
        (style.color >> 16) as u8,
    ];
    let opacity = style.opacity.clamp(0., 1.);
    let mut crossings = Vec::new();
    for y in region.y..region.y + region.height {
        coverage.scan_row(
            y,
            region.x,
            region.x + region.width,
            &mut crossings,
            |x, value| {
                crate::raster::over_pixel(pixel_mut(target, x, y), &color, value * opacity);
            },
        );
    }
    Ok(())
}

fn paint_cached(target: &mut Image, rect: Rect, image: &Image, projection: &OutputProjection) {
    let Some(region) = projection.bounds(rect_bounds(rect), 0.) else {
        return;
    };
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            let point = projection.source_center(x, y);
            if !inside(rect, point) {
                continue;
            }
            let index = ((point.y.floor() as u32 - rect.y) as usize * image.width as usize
                + (point.x.floor() as u32 - rect.x) as usize)
                * 4;
            if let Some(pixel) = image.bgra.get(index..index + 4) {
                crate::raster::over_pixel(pixel_mut(target, x, y), pixel, pixel[3] as f32 / 255.);
            }
        }
    }
}

fn paint_highlighter(
    target: &mut Image,
    mesh: &VectorMesh,
    style: Style,
    tiles: &[HighlighterTile],
    projection: &OutputProjection,
) -> Result<()> {
    let mesh = mesh.exact()?;
    let Some(region) = mesh_region(mesh, projection)? else {
        return Ok(());
    };
    let coverage = MeshCoverage::projected(mesh, projection, region)?;
    let mut crossings = Vec::new();
    // 一次进入一个tile的目标core，不在每个输出像素线性搜索所有source tiles。
    for tile in tiles {
        let Some(part) = projection
            .bounds(rect_bounds(tile.rect), 0.)
            .and_then(|part| part.intersection(region))
        else {
            continue;
        };
        for y in part.y..part.y + part.height {
            // 2048源网格不随目标倍率变化。分数目标边界以源半开core决定唯一归属。
            coverage.scan_row(
                y,
                part.x,
                part.x + part.width,
                &mut crossings,
                |x, value| {
                    if value == 0. {
                        return;
                    }
                    let point = projection.source_center(x, y);
                    if !inside(tile.rect, point) || !inside(tile.image_rect, point) {
                        return;
                    }
                    let index = ((point.y.floor() as u32 - tile.image_rect.y) as usize
                        * tile.image.width as usize
                        + (point.x.floor() as u32 - tile.image_rect.x) as usize)
                        * 4;
                    if let Some(color) = tile.image.bgra.get(index..index + 4) {
                        crate::raster::over_pixel(
                            pixel_mut(target, x, y),
                            color,
                            value * style.opacity.clamp(0., 1.) * color[3] as f32 / 255.,
                        );
                    }
                },
            );
        }
    }
    Ok(())
}

fn restore(
    target: &mut Image,
    a: &Annotation,
    source: &SourcePixels<'_>,
    projection: &OutputProjection,
) -> Result<()> {
    // 原恢复层缓存仍供1×合成；目标层只恢复笔迹实际覆盖范围，其余高清下层保持原值。
    let mesh = VectorMesh::build(&a.vector_path(), a.style, false)?;
    let Some(region) = mesh_region(&mesh, projection)? else {
        return Ok(());
    };
    let coverage = MeshCoverage::projected(&mesh, projection, region)?;
    let mut crossings = Vec::new();
    for y in region.y..region.y + region.height {
        coverage.scan_row(
            y,
            region.x,
            region.x + region.width,
            &mut crossings,
            |x, value| {
                let t = value * a.style.opacity.clamp(0., 1.);
                if t == 0. {
                    return;
                }
                let original = source.sample(projection.source_center(x, y));
                let old = pixel_mut(target, x, y);
                let old_alpha = old[3] as f32 / 255.;
                let original_alpha = original[3] as f32 / 255.;
                let alpha = old_alpha * (1. - t) + original_alpha * t;
                for channel in 0..3 {
                    old[channel] = if alpha > 0. {
                        ((old[channel] as f32 * old_alpha * (1. - t)
                            + original[channel] as f32 * original_alpha * t)
                            / alpha)
                            .round() as u8
                    } else {
                        0
                    };
                }
                old[3] = (alpha * 255.).round() as u8;
            },
        );
    }
    Ok(())
}

pub(crate) fn paint_spotlight(
    geometry: &SpotlightGeometry,
    target: &mut Image,
    projection: &OutputProjection,
) -> Result<()> {
    let Some(region) = projection.bounds(rect_bounds(geometry.source_canvas()), 0.) else {
        return Ok(());
    };
    let holes = MeshCoverage::from_source_edges(geometry.hole_edges(), projection, region)?;
    let border = MeshCoverage::projected(&geometry.border, projection, region)?;
    let color = [
        geometry.style.color as u8,
        (geometry.style.color >> 8) as u8,
        (geometry.style.color >> 16) as u8,
    ];
    let mut crossings = Vec::new();
    for y in region.y..region.y + region.height {
        // 暗化从孔的并集反算，不把输出边缘/画布裁剪边缘当作新的半透明外边。
        holes.scan_row(
            y,
            region.x,
            region.x + region.width,
            &mut crossings,
            |x, value| {
                if inside(geometry.source_canvas(), projection.source_center(x, y)) {
                    crate::raster::over_pixel(
                        pixel_mut(target, x, y),
                        &[0, 0, 0],
                        (1. - value) * geometry.style.opacity.clamp(0., 1.),
                    );
                }
            },
        );
        border.scan_row(
            y,
            region.x,
            region.x + region.width,
            &mut crossings,
            |x, value| {
                if inside(geometry.source_canvas(), projection.source_center(x, y)) {
                    crate::raster::over_pixel(pixel_mut(target, x, y), &color, value);
                }
            },
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "projected_scene_tests.rs"]
mod tests;
