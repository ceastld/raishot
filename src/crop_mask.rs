//! 非零环绕套索：全屏物理坐标、不可变轮廓与共用抗锯齿覆盖率。
use crate::{
    annotation::{Bounds, Point},
    capture::Image,
    geometry::Rect,
};
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    pub points: Arc<[Point]>,
    pub triangles: Arc<[[Point; 3]]>,
    pub bounds: Bounds,
}
impl Polygon {
    pub fn new(raw: &[Point]) -> Option<Self> {
        let mut points = Vec::new();
        for &p in raw {
            if !p.x.is_finite() || !p.y.is_finite() {
                return None;
            }
            if points.last().is_none_or(|q: &Point| q.distance(p) > 0.25) {
                points.push(p);
            }
        }
        if points.len() > 1 && points[0].distance(*points.last().unwrap()) <= 0.25 {
            points.pop();
        }
        if points.len() < 3 {
            return None;
        }
        let bounds = crate::transform::bounds(points.iter().copied());
        if bounds.width < 5. || bounds.height < 5. {
            return None;
        }
        // 非零环绕先生成无重叠三角片，预览反向遮罩可使用偶奇规则而不误挖双绕圈。
        let mut builder = lyon_path::Path::builder();
        builder.begin(lyon_path::math::point(points[0].x, points[0].y));
        for p in &points[1..] {
            builder.line_to(lyon_path::math::point(p.x, p.y));
        }
        builder.end(true);
        let path = builder.build();
        let mut mesh = lyon_tessellation::VertexBuffers::<Point, u32>::new();
        let mut tess = lyon_tessellation::FillTessellator::new();
        tess.tessellate_path(
            &path,
            &lyon_tessellation::FillOptions::default()
                .with_fill_rule(lyon_path::FillRule::NonZero)
                .with_tolerance(0.03),
            &mut lyon_tessellation::BuffersBuilder::new(
                &mut mesh,
                |v: lyon_tessellation::FillVertex<'_>| {
                    let p = v.position();
                    Point { x: p.x, y: p.y }
                },
            ),
        )
        .ok()?;
        let triangles: Vec<_> = mesh
            .indices
            .chunks_exact(3)
            .map(|i| {
                [
                    mesh.vertices[i[0] as usize],
                    mesh.vertices[i[1] as usize],
                    mesh.vertices[i[2] as usize],
                ]
            })
            .collect();
        let area: f32 = triangles
            .iter()
            .map(|[a, b, c]| ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() / 2.)
            .sum();
        if area < 1. {
            return None;
        }
        Some(Self {
            points: points.into(),
            triangles: triangles.into(),
            bounds,
        })
    }
    pub fn output_rect(&self, host: Rect) -> Option<Rect> {
        {
            let x = self.bounds.x.floor().max(host.x as f32) as u32;
            let y = self.bounds.y.floor().max(host.y as f32) as u32;
            let right = (self.bounds.x + self.bounds.width)
                .ceil()
                .max(0.)
                .min((host.x + host.width) as f32) as u32;
            let bottom = (self.bounds.y + self.bounds.height)
                .ceil()
                .max(0.)
                .min((host.y + host.height) as f32) as u32;
            (x < right && y < bottom).then(|| Rect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            })
        }
    }
    pub fn coverage(&self, region: Rect) -> Vec<u8> {
        let mut coverage = vec![0_u8; region.width as usize * region.height as usize];
        // 浮点工作区只保留一行，避免为整屏额外分配四字节 alpha 平面。
        let mut row = vec![0_f32; region.width as usize];
        let mut crossings = Vec::with_capacity(self.points.len());
        for y in region.y..region.y + region.height {
            row.fill(0.);
            for offset in [0.125, 0.375, 0.625, 0.875] {
                let scan = y as f32 + offset;
                crossings.clear();
                for i in 0..self.points.len() {
                    let a = self.points[i];
                    let b = self.points[(i + 1) % self.points.len()];
                    let winding = if a.y <= scan && b.y > scan {
                        1
                    } else if b.y <= scan && a.y > scan {
                        -1
                    } else {
                        continue;
                    };
                    let x = a.x + (scan - a.y) * (b.x - a.x) / (b.y - a.y);
                    crossings.push((x, winding));
                }
                crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut winding = 0;
                let mut start = 0.;
                for &(x, d) in &crossings {
                    let old = winding;
                    winding += d;
                    if old == 0 && winding != 0 {
                        start = x;
                    } else if old != 0 && winding == 0 {
                        let left = start.max(region.x as f32);
                        let right = x.min((region.x + region.width) as f32);
                        if right > left {
                            for col in left.floor() as u32..right.ceil() as u32 {
                                let amount = (right.min(col as f32 + 1.) - left.max(col as f32))
                                    .max(0.)
                                    * 0.25;
                                row[(col - region.x) as usize] += amount;
                            }
                        }
                    }
                }
            }
            let begin = ((y - region.y) * region.width) as usize;
            for (out, &c) in coverage[begin..begin + region.width as usize]
                .iter_mut()
                .zip(&row)
            {
                *out = (c.clamp(0., 1.) * 255.).round() as u8;
            }
        }
        coverage
    }
    pub fn apply(&self, image: &mut Image, region: Rect) {
        for (pixel, c) in image.bgra.chunks_exact_mut(4).zip(self.coverage(region)) {
            pixel[3] = ((pixel[3] as u32 * c as u32 + 127) / 255) as u8;
            if pixel[3] == 0 {
                pixel.fill(0);
            }
        }
    }
    /// 在输出像素中重新计算面积覆盖率，仅保留一行临时数组。
    pub(crate) fn apply_projected(
        &self,
        image: &mut Image,
        projection: &crate::output_projection::OutputProjection,
    ) {
        let points: Vec<_> = self.points.iter().map(|&p| projection.map(p)).collect();
        let mut row = vec![0_f32; image.width as usize];
        let mut crossings = Vec::with_capacity(points.len());
        for y in 0..image.height {
            row.fill(0.);
            for offset in [0.125, 0.375, 0.625, 0.875] {
                let scan = y as f32 + offset;
                crossings.clear();
                for n in 0..points.len() {
                    let a = points[n];
                    let b = points[(n + 1) % points.len()];
                    let winding = if a.y <= scan && b.y > scan {
                        1
                    } else if b.y <= scan && a.y > scan {
                        -1
                    } else {
                        continue;
                    };
                    crossings.push((a.x + (scan - a.y) * (b.x - a.x) / (b.y - a.y), winding));
                }
                crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut winding = 0;
                let mut start = 0.;
                for &(x, d) in &crossings {
                    let old = winding;
                    winding += d;
                    if old == 0 && winding != 0 {
                        start = x;
                    } else if old != 0 && winding == 0 {
                        let left = start.max(0.);
                        let right = x.min(image.width as f32);
                        if right > left {
                            for col in left.floor() as u32..right.ceil() as u32 {
                                row[col as usize] +=
                                    (right.min(col as f32 + 1.) - left.max(col as f32)).max(0.)
                                        * 0.25;
                            }
                        }
                    }
                }
            }
            let start = y as usize * image.width as usize * 4;
            for (p, c) in image.bgra[start..start + image.width as usize * 4]
                .chunks_exact_mut(4)
                .zip(&row)
            {
                let coverage = (c.clamp(0., 1.) * 255.).round() as u32;
                p[3] = ((p[3] as u32 * coverage + 127) / 255) as u8;
                if p[3] == 0 {
                    p.fill(0);
                }
            }
        }
    }
}
