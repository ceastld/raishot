//! CPU 目标像素覆盖率：只投影外边界，不分配全幅 mask 或浮点画布。
use crate::{
    annotation::{distance_to_segment, Point},
    geometry::Rect,
    output_projection::OutputProjection,
    vector_mesh::VectorMesh,
    Result,
};
use std::collections::HashMap;

const CELL: f32 = 64.;
pub(crate) struct MeshCoverage {
    // 保留完整边，裁剪左侧的交点仍决定 inside；距离索引只保留兴趣区附近的边。
    edges: Vec<(Point, Point)>,
    close_edges: Vec<(Point, Point)>,
    nearby: HashMap<(i32, i32), Vec<usize>>,
}
impl MeshCoverage {
    pub(crate) fn projected(
        mesh: &VectorMesh,
        projection: &OutputProjection,
        interest: Rect,
    ) -> Result<Self> {
        let mut edges = Vec::with_capacity(mesh.edges.len());
        for &(a, b) in &mesh.edges {
            let a = *mesh
                .vertices
                .get(a as usize)
                .ok_or("投影网格的边索引无效")?;
            let b = *mesh
                .vertices
                .get(b as usize)
                .ok_or("投影网格的边索引无效")?;
            edges.push((a, b));
        }
        Self::from_source_edges(&edges, projection, interest)
    }
    pub(crate) fn from_source_edges(
        source: &[(Point, Point)],
        projection: &OutputProjection,
        interest: Rect,
    ) -> Result<Self> {
        let mut edges = Vec::with_capacity(source.len());
        let mut close_edges = Vec::new();
        let mut nearby = HashMap::<(i32, i32), Vec<usize>>::new();
        for &(a, b) in source {
            let (a, b) = (projection.map(a), projection.map(b));
            if [a.x, a.y, b.x, b.y].iter().any(|v| !v.is_finite()) {
                return Err("投影轮廓坐标不是有限数值".into());
            }
            edges.push((a, b));
            let Some((a, b)) = clip_edge(a, b, interest) else {
                continue;
            };
            let index = close_edges.len();
            close_edges.push((a, b));
            let steps = ((a.x - b.x).abs().max((a.y - b.y).abs()) / CELL)
                .ceil()
                .max(1.) as usize;
            for step in 0..=steps {
                let t = step as f32 / steps as f32;
                let key = cell(Point {
                    x: a.x + (b.x - a.x) * t,
                    y: a.y + (b.y - a.y) * t,
                });
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let bucket = nearby.entry((key.0 + dx, key.1 + dy)).or_default();
                        if bucket.last() != Some(&index) {
                            bucket.push(index);
                        }
                    }
                }
            }
        }
        Ok(Self {
            edges,
            close_edges,
            nearby,
        })
    }
    /// 顺序扫描一行，交点 Vec 由调用者复用；无目标宽×高临时平面。
    pub(crate) fn scan_row(
        &self,
        y: u32,
        left: u32,
        right: u32,
        intersections: &mut Vec<f32>,
        mut paint: impl FnMut(u32, f32),
    ) {
        let py = y as f32 + 0.5;
        intersections.clear();
        for &(a, b) in &self.edges {
            if (a.y > py) != (b.y > py) {
                // f64 防止位于裁剪范围外的长边斜率在 f32 乘法中溢出。
                let t = (f64::from(py) - f64::from(a.y)) / (f64::from(b.y) - f64::from(a.y));
                intersections.push((f64::from(a.x) + (f64::from(b.x) - f64::from(a.x)) * t) as f32);
            }
        }
        intersections.sort_unstable_by(f32::total_cmp);
        let mut index = intersections.partition_point(|&v| v <= left as f32 + 0.5);
        for x in left..right {
            let point = Point {
                x: x as f32 + 0.5,
                y: py,
            };
            while index < intersections.len() && intersections[index] <= point.x {
                index += 1;
            }
            let mut distance = 0.5_f32;
            if let Some(edges) = self.nearby.get(&cell(point)) {
                for &i in edges {
                    let (a, b) = self.close_edges[i];
                    if point.x < a.x.min(b.x) - 0.5
                        || point.x > a.x.max(b.x) + 0.5
                        || point.y < a.y.min(b.y) - 0.5
                        || point.y > a.y.max(b.y) + 0.5
                    {
                        continue;
                    }
                    distance = distance.min(distance_to_segment(point, a, b));
                }
            }
            let coverage = (0.5 + if index % 2 == 1 { distance } else { -distance }).clamp(0., 1.);
            paint(x, coverage);
        }
    }
}
fn cell(p: Point) -> (i32, i32) {
    ((p.x / CELL).floor() as i32, (p.y / CELL).floor() as i32)
}

/// Liang–Barsky 只索引可影响兴趣区 AA 的边段；巨大外部几何不会生成整条边的桶。
fn clip_edge(a: Point, b: Point, interest: Rect) -> Option<(Point, Point)> {
    let (ax, ay, bx, by) = (
        f64::from(a.x),
        f64::from(a.y),
        f64::from(b.x),
        f64::from(b.y),
    );
    let (dx, dy) = (bx - ax, by - ay);
    let mut start = 0_f64;
    let mut end = 1_f64;
    for (direction, distance) in [
        (-dx, ax - f64::from(interest.x) + 1.),
        (
            dx,
            f64::from(interest.x) + f64::from(interest.width) + 1. - ax,
        ),
        (-dy, ay - f64::from(interest.y) + 1.),
        (
            dy,
            f64::from(interest.y) + f64::from(interest.height) + 1. - ay,
        ),
    ] {
        if direction == 0. {
            if distance < 0. {
                return None;
            }
            continue;
        }
        let t = distance / direction;
        if direction < 0. {
            start = start.max(t);
        } else {
            end = end.min(t);
        }
        if start > end {
            return None;
        }
    }
    let point = |t: f64| Point {
        x: (ax + dx * t) as f32,
        y: (ay + dy * t) as f32,
    };
    Some((point(start), point(end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(x: f32, y: f32) -> Point {
        Point { x, y }
    }
    fn rectangle(left: f32, top: f32, right: f32, bottom: f32) -> Vec<(Point, Point)> {
        let a = [
            p(left, top),
            p(right, top),
            p(right, bottom),
            p(left, bottom),
        ];
        (0..4).map(|i| (a[i], a[(i + 1) % 4])).collect()
    }
    #[test]
    fn enlarged_export_coverage_giant_external_geometry_indexes_only_interest() {
        let projection = OutputProjection::new(
            Rect {
                x: 100,
                y: 100,
                width: 8,
                height: 8,
            },
            2.,
        )
        .unwrap();
        let coverage = MeshCoverage::from_source_edges(
            &rectangle(-100_000_000., -100_000_000., 100_000_000., 100_000_000.),
            &projection,
            Rect::full(16, 16),
        )
        .unwrap();
        assert!(coverage.close_edges.is_empty());
        assert!(coverage.nearby.is_empty());
        let mut crossings = Vec::new();
        let mut values = Vec::new();
        coverage.scan_row(8, 0, 16, &mut crossings, |_, v| values.push(v));
        assert_eq!(values, vec![1.; 16], "远处左右边仍决定裁剪区域在几何内部");
        let long = MeshCoverage::from_source_edges(
            &rectangle(-100_000_000., 103.125, 100_000_000., 106.),
            &projection,
            Rect::full(16, 16),
        )
        .unwrap();
        assert!(long.nearby.len() < 25, "长边空间桶必须只覆盖16px兴趣区");
        let mut values = Vec::new();
        long.scan_row(6, 0, 16, &mut crossings, |_, v| values.push(v));
        assert!(values.iter().all(|&v| (v - 0.75).abs() < 0.001));
    }
    #[test]
    fn enlarged_export_coverage_nonuniform_projection_has_target_pixel_aa() {
        let projection = OutputProjection::new(Rect::full(1, 3), 1.5).unwrap();
        let coverage = MeshCoverage::from_source_edges(
            &rectangle(0.375, 0., 1., 3.),
            &projection,
            Rect::full(2, 4),
        )
        .unwrap();
        let mut crossings = Vec::new();
        let mut values = Vec::new();
        coverage.scan_row(1, 0, 2, &mut crossings, |_, v| values.push(v));
        assert!(
            (values[0] - 0.25).abs() < 0.001,
            "sx=2，源x=.375应落目标x=.75"
        );
        assert_eq!(values[1], 1.);
    }
}
