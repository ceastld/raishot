//! 聚光灯共享几何：先做孔洞并集，再绘制一次暗层与并集外轮廓。
//! 预览使用矢量网格；导出沿同一网格边界计算覆盖率，只扫描请求区域。
use crate::{
    annotation::{distance_to_segment, Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    vector_mesh::VectorMesh,
    vector_path::VectorPath,
    Result,
};
use std::collections::HashMap;

pub struct SpotlightGeometry {
    pub mask: VectorMesh,
    pub border: VectorMesh,
    pub style: Style,
    holes: Coverage,
    outline: Coverage,
    canvas: Rect,
}

impl SpotlightGeometry {
    pub(crate) fn source_canvas(&self) -> Rect {
        self.canvas
    }
    pub(crate) fn hole_edges(&self) -> &[(Point, Point)] {
        &self.holes.edges
    }
    pub(crate) fn apply_projected(
        &self,
        target: &mut Image,
        projection: &crate::output_projection::OutputProjection,
    ) -> Result<()> {
        crate::projected_scene::paint_spotlight(self, target, projection)
    }

    pub fn build(items: &[Annotation], canvas: Rect) -> Result<Self> {
        let mut style = items
            .iter()
            .rev()
            .find(|a| a.tool == Tool::Spotlight)
            .ok_or("聚光灯集合为空")?
            .style;
        if !style.width.is_finite() || !style.opacity.is_finite() {
            return Err("聚光灯样式不是有限数值".into());
        }
        style.width = style.width.clamp(0., 512.);
        style.opacity = style.opacity.clamp(0., 1.);
        let mut contours = Vec::new();
        for a in items.iter().filter(|a| a.tool == Tool::Spotlight) {
            let b = a.local_bounds();
            if b.width <= 0. || b.height <= 0. {
                continue;
            }
            let rotation = a.rotation_transform();
            let mut contour: Vec<_> = VectorPath::rectangle(b, a.style.radius)
                .flattened()
                .iter()
                .map(|&(p, _)| rotation.map(p))
                .collect();
            if contour.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
                return Err("聚光灯坐标不是有限数值".into());
            }
            if area(&contour) < 0. {
                contour.reverse();
            }
            contours.push(contour);
        }
        // NonZero 同绕向填充是真正并集；重叠孔不会形成 EvenOdd 反转。
        let union = VectorMesh::fill_contours(&contours)?;
        let boundaries = union.boundary_contours()?;
        let border = if style.width > 0. {
            let mut path = VectorPath::default();
            for contour in &boundaries {
                path.spans
                    .extend(VectorPath::polyline(contour, false, true).spans);
            }
            VectorMesh::build(
                &path,
                Style {
                    width: style.width,
                    opacity: 1.,
                    dashed: false,
                    filled: false,
                    ..style
                },
                false,
            )?
        } else {
            VectorMesh::default()
        };
        // 先裁孔再扣除，避免超出画布的反向轮廓在画布外形成负绕向填充。
        // 边框仍使用未裁孔的原始并集，画布交线不会成为额外边框。
        let clipped: Vec<_> = contours
            .iter()
            .map(|contour| clip(contour, canvas))
            .filter(|contour| contour.len() >= 3)
            .collect();
        let clipped_union = VectorMesh::fill_contours(&clipped)?;
        let mut mask_contours = vec![vec![
            p(canvas.x as f32, canvas.y as f32),
            p((canvas.x + canvas.width) as f32, canvas.y as f32),
            p(
                (canvas.x + canvas.width) as f32,
                (canvas.y + canvas.height) as f32,
            ),
            p(canvas.x as f32, (canvas.y + canvas.height) as f32),
        ]];
        for mut contour in clipped_union.boundary_contours()? {
            contour.reverse();
            mask_contours.push(contour);
        }
        let mask = VectorMesh::fill_contours(&mask_contours)?;
        Ok(Self {
            holes: Coverage::new(&union),
            outline: Coverage::new(&border),
            mask,
            border,
            style,
            canvas,
        })
    }

    pub fn mask_style(&self) -> Style {
        Style {
            color: 0,
            width: 1.,
            opacity: self.style.opacity.clamp(0., 1.),
            ..self.style
        }
    }

    pub fn border_style(&self) -> Style {
        Style {
            opacity: 1.,
            ..self.style
        }
    }

    pub fn apply(&self, image: &mut Image, region: Rect) {
        let Some(part) = self.canvas.intersection(region) else {
            return;
        };
        let opacity = self.style.opacity.clamp(0., 1.);
        let color = [
            self.style.color as u8,
            (self.style.color >> 8) as u8,
            (self.style.color >> 16) as u8,
        ];
        let mut holes = Vec::new();
        let mut border = Vec::new();
        for y in part.y..part.y + part.height {
            let py = y as f32 + 0.5;
            self.holes.intersections(py, &mut holes);
            self.outline.intersections(py, &mut border);
            let (mut hi, mut bi) = (0, 0);
            for x in part.x..part.x + part.width {
                let point = p(x as f32 + 0.5, py);
                while hi < holes.len() && holes[hi] <= point.x {
                    hi += 1;
                }
                while bi < border.len() && border[bi] <= point.x {
                    bi += 1;
                }
                let offset = (((y - region.y) * image.width + x - region.x) as usize) * 4;
                let pixel = &mut image.bgra[offset..offset + 4];
                let hole = self.holes.coverage(point, hi % 2 == 1);
                crate::raster::over_pixel(pixel, &[0, 0, 0], (1. - hole) * opacity);
                let edge = self.outline.coverage(point, bi % 2 == 1);
                crate::raster::over_pixel(pixel, &color, edge);
            }
        }
    }
}

fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}

fn area(contour: &[Point]) -> f32 {
    contour
        .iter()
        .zip(contour.iter().cycle().skip(1))
        .take(contour.len())
        .map(|(a, b)| a.x * b.y - a.y * b.x)
        .sum::<f32>()
        * 0.5
}

/// 凸孔的 Sutherland–Hodgman 裁剪；圆角折线和旋转均保持原来的绕向。
fn clip(contour: &[Point], rect: Rect) -> Vec<Point> {
    let mut result = contour.to_vec();
    for (axis, limit, greater) in [
        (0, rect.x as f32, true),
        (0, (rect.x + rect.width) as f32, false),
        (1, rect.y as f32, true),
        (1, (rect.y + rect.height) as f32, false),
    ] {
        let coordinate = |p: Point| if axis == 0 { p.x } else { p.y };
        let inside = |p: Point| {
            if greater {
                coordinate(p) >= limit
            } else {
                coordinate(p) <= limit
            }
        };
        let mut next = Vec::new();
        if let Some(&last) = result.last() {
            let mut a = last;
            for &b in &result {
                if inside(a) != inside(b) {
                    let t = (limit - coordinate(a)) / (coordinate(b) - coordinate(a));
                    next.push(p(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t));
                }
                if inside(b) {
                    next.push(b);
                }
                a = b;
            }
        }
        result = next;
    }
    result
}

const CELL: f32 = 64.;
struct Coverage {
    edges: Vec<(Point, Point)>,
    nearby: HashMap<(i32, i32), Vec<usize>>,
}

impl Coverage {
    fn new(mesh: &VectorMesh) -> Self {
        let edges: Vec<_> = mesh
            .edges
            .iter()
            .map(|&(a, b)| (mesh.vertices[a as usize], mesh.vertices[b as usize]))
            .collect();
        let mut nearby = HashMap::<(i32, i32), Vec<usize>>::new();
        for (i, &(a, b)) in edges.iter().enumerate() {
            // 沿边走网格并扩一个邻格；不为斜长边分配整片包围盒。
            let steps = ((a.x - b.x).abs().max((a.y - b.y).abs()) / CELL)
                .ceil()
                .max(1.) as usize;
            for step in 0..=steps {
                let t = step as f32 / steps as f32;
                let key = cell(p(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t));
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let bucket = nearby.entry((key.0 + dx, key.1 + dy)).or_default();
                        if bucket.last() != Some(&i) {
                            bucket.push(i);
                        }
                    }
                }
            }
        }
        Self { edges, nearby }
    }

    fn intersections(&self, y: f32, out: &mut Vec<f32>) {
        out.clear();
        for &(a, b) in &self.edges {
            if (a.y > y) != (b.y > y) {
                out.push(a.x + (y - a.y) * (b.x - a.x) / (b.y - a.y));
            }
        }
        out.sort_unstable_by(f32::total_cmp);
    }

    fn coverage(&self, point: Point, inside: bool) -> f32 {
        let mut distance = 0.5_f32;
        if let Some(edges) = self.nearby.get(&cell(point)) {
            for &i in edges {
                let (a, b) = self.edges[i];
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
        (0.5 + if inside { distance } else { -distance }).clamp(0., 1.)
    }
}

fn cell(p: Point) -> (i32, i32) {
    ((p.x / CELL).floor() as i32, (p.y / CELL).floor() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hole(a: (f32, f32), b: (f32, f32), radius: f32, width: f32) -> Annotation {
        let mut item = Annotation::new(
            Tool::Spotlight,
            p(a.0, a.1),
            Style {
                radius,
                width,
                color: 0xffffff,
                opacity: 0.5,
                ..Default::default()
            },
        );
        item.update(p(b.0, b.1));
        item
    }
    fn render(g: &SpotlightGeometry, region: Rect) -> Image {
        let mut image = Image {
            width: region.width,
            height: region.height,
            bgra: [100, 100, 100, 255].repeat(region.width as usize * region.height as usize),
            readback_ms: 0.,
        };
        g.apply(&mut image, region);
        image
    }
    fn value(image: &Image, x: u32, y: u32) -> u8 {
        image.bgra[((y * image.width + x) * 4) as usize]
    }
    #[test]
    fn spotlight_union_ignores_internal_seams_and_dims_once() {
        let geometry = SpotlightGeometry::build(
            &[
                hole((8., 8.), (32., 32.), 0., 2.),
                hole((20., 8.), (44., 32.), 0., 2.),
            ],
            Rect::full(56, 40),
        )
        .unwrap();
        let image = render(&geometry, Rect::full(56, 40));
        assert_eq!(value(&image, 2, 2), 50);
        for x in [10, 20, 31, 40] {
            assert_eq!(value(&image, x, 20), 100, "内部边框或重复暗化 x={x}");
        }
        assert_eq!(value(&image, 8, 20), 255);
    }
    #[test]
    fn spotlight_rounded_rotated_edges_are_aa_and_crop_exact() {
        let mut item = hole((12., 12.), (46., 40.), 50., 0.);
        item.rotation = 23.;
        let geometry = SpotlightGeometry::build(&[item], Rect::full(64, 56)).unwrap();
        assert!(geometry.border.indices.is_empty());
        let image = render(&geometry, Rect::full(64, 56));
        let shades: std::collections::HashSet<_> =
            image.bgra.chunks_exact(4).map(|p| p[0]).collect();
        assert!(shades.iter().filter(|&&v| v > 50 && v < 100).count() > 8);
        let crop = Rect {
            x: 7,
            y: 9,
            width: 47,
            height: 35,
        };
        let part = render(&geometry, crop);
        for y in 0..crop.height {
            for x in 0..crop.width {
                assert_eq!(value(&part, x, y), value(&image, x + crop.x, y + crop.y));
            }
        }
    }
    #[test]
    fn spotlight_canvas_clipping_has_no_fake_border_and_transparency_is_straight() {
        let mut item = hole((-8., 8.), (20., 28.), 0., 2.);
        item.style.opacity = 0.;
        let geometry = SpotlightGeometry::build(&[item], Rect::full(40, 40)).unwrap();
        let image = render(&geometry, Rect::full(40, 40));
        assert_eq!(value(&image, 0, 18), 100, "画布交线不应生成边框");
        assert_eq!(value(&image, 19, 18), 255, "暗化为0仍须显示边框");
        let geometry =
            SpotlightGeometry::build(&[hole((8., 8.), (24., 24.), 0., 0.)], Rect::full(32, 32))
                .unwrap();
        let mut transparent = Image {
            width: 32,
            height: 32,
            bgra: vec![0; 32 * 32 * 4],
            readback_ms: 0.,
        };
        geometry.apply(&mut transparent, Rect::full(32, 32));
        assert_eq!(&transparent.bgra[..4], &[0, 0, 0, 128]);
        assert_eq!(transparent.bgra[((16 * 32 + 16) * 4 + 3) as usize], 0);
        let geometry = SpotlightGeometry::build(
            &[hole((64., 64.), (192., 192.), 0., 80.)],
            Rect::full(256, 256),
        )
        .unwrap();
        assert_eq!(geometry.border_style().width, 80.);
        assert!(geometry.border.vertices.iter().any(|p| p.x < 30.));
    }
    #[test]
    fn spotlight_disjoint_touching_and_enclosed_union_preserve_correct_winding() {
        let canvas = Rect::full(64, 64);
        let geometry = SpotlightGeometry::build(
            &[
                hole((4., 4.), (20., 20.), 0., 0.),
                hole((20., 4.), (36., 20.), 0., 0.),
                hole((44., 4.), (60., 20.), 0., 0.),
            ],
            canvas,
        )
        .unwrap();
        let image = render(&geometry, canvas);
        assert_eq!(value(&image, 20, 12), 100);
        assert_eq!(value(&image, 40, 12), 50);
        assert_eq!(value(&image, 50, 12), 100);
        let geometry = SpotlightGeometry::build(
            &[
                hole((8., 8.), (56., 20.), 0., 0.),
                hole((8., 44.), (56., 56.), 0., 0.),
                hole((8., 16.), (20., 48.), 0., 0.),
                hole((44., 16.), (56., 48.), 0., 0.),
            ],
            canvas,
        )
        .unwrap();
        let image = render(&geometry, canvas);
        assert_eq!(value(&image, 12, 32), 100);
        assert_eq!(value(&image, 32, 32), 50, "并集围出的空隙仍须暗化");
        assert_eq!(value(&image, 2, 2), 50);
    }
}
