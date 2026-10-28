//! 字形、圆形描边与背景只合成一次对象透明度；预览与导出共用。
use crate::{annotation::Annotation, capture::Image, geometry::Rect};
pub fn paint(a: &Annotation, rect: Rect, mask: &[u8]) -> Image {
    let mut image = crate::raster::empty(rect);
    let style = a.style.text;
    if style.background && a.tool == crate::annotation::Tool::Text {
        let b = a.local_bounds();
        let r = style
            .background_radius
            .min(b.width.min(b.height) / 2.)
            .max(0.);
        for y in 0..rect.height {
            for x in 0..rect.width {
                let qx = ((x + rect.x) as f32 + 0.5 - b.x - b.width / 2.).abs() - b.width / 2. + r;
                let qy =
                    ((y + rect.y) as f32 + 0.5 - b.y - b.height / 2.).abs() - b.height / 2. + r;
                let d = qx.max(0.).hypot(qy.max(0.)) + qx.max(qy).min(0.) - r;
                over(
                    &mut image.bgra[((y * rect.width + x) * 4) as usize..][..4],
                    style.background_color,
                    (0.5 - d).clamp(0., 1.) * style.background_opacity,
                );
            }
        }
    }
    if style.outline {
        let outline = dilate(
            mask,
            rect.width as usize,
            rect.height as usize,
            style.outline_width,
        );
        for (p, coverage) in image.bgra.chunks_exact_mut(4).zip(outline) {
            over(p, style.outline_color, coverage as f32 / 255.);
        }
    }
    if !style.outline && !(style.background && a.tool == crate::annotation::Tool::Text) {
        // 底图全透明且只有字形：像素只由覆盖率决定，按 256 级查表，与逐像素合成逐字节相同。
        let table = plain_table(a.style.color, a.style.opacity);
        for (p, &coverage) in image.bgra.chunks_exact_mut(4).zip(mask) {
            if coverage != 0 {
                p.copy_from_slice(&table[coverage as usize]);
            }
        }
        return image;
    }
    for (p, &coverage) in image.bgra.chunks_exact_mut(4).zip(mask) {
        over(p, a.style.color, coverage as f32 / 255.);
        p[3] = (p[3] as f32 * a.style.opacity).round() as u8;
    }
    image
}
fn plain_table(color: u32, opacity: f32) -> Vec<[u8; 4]> {
    (0..=255u8)
        .map(|coverage| {
            let mut p = [0u8; 4];
            over(&mut p, color, coverage as f32 / 255.);
            p[3] = (p[3] as f32 * opacity).round() as u8;
            p
        })
        .collect()
}
fn over(p: &mut [u8], color: u32, alpha: f32) {
    if alpha <= 0. {
        return;
    }
    let old = p[3] as f32 / 255.;
    let next = alpha + old * (1. - alpha);
    for (c, dst) in p[..3].iter_mut().enumerate() {
        *dst = ((((color >> (c * 8)) & 255) as f32 * alpha + *dst as f32 * old * (1. - alpha))
            / next)
            .round() as u8;
    }
    p[3] = (next * 255.).round() as u8;
}
// 两遍欧氏距离变换，复杂度与图层面积线性相关，不随描边半径平方增长。
fn dilate(mask: &[u8], w: usize, h: usize, radius: f32) -> Vec<u8> {
    if radius <= 0. {
        return mask.to_vec();
    }
    const INF: f32 = 1e12;
    let mut distance = vec![INF; mask.len()];
    let mut input = vec![INF; w.max(h)];
    let mut output = vec![INF; w.max(h)];
    for x in 0..w {
        for y in 0..h {
            let m = mask[y * w + x];
            input[y] = if m == 0 {
                INF
            } else {
                (1. - m as f32 / 255.).powi(2)
            };
        }
        transform(&input[..h], &mut output[..h]);
        for y in 0..h {
            distance[y * w + x] = output[y];
        }
    }
    for y in 0..h {
        input[..w].copy_from_slice(&distance[y * w..(y + 1) * w]);
        transform(&input[..w], &mut output[..w]);
        distance[y * w..(y + 1) * w].copy_from_slice(&output[..w]);
    }
    distance
        .into_iter()
        .map(|d| ((radius + 1. - d.sqrt()).clamp(0., 1.) * 255.).round() as u8)
        .collect()
}
fn transform(input: &[f32], out: &mut [f32]) {
    let mut sites = Vec::<usize>::new();
    let mut boundaries = Vec::<f32>::new();
    for (q, &f) in input.iter().enumerate().filter(|(_, f)| **f < 1e12) {
        let mut edge = f32::NEG_INFINITY;
        while let Some(&p) = sites.last() {
            edge = (f + ((q - p) * (q + p)) as f32 - input[p]) / (2 * (q - p)) as f32;
            if sites.len() == 1 || edge > boundaries[sites.len() - 1] {
                break;
            }
            sites.pop();
            boundaries.pop();
        }
        if sites.is_empty() {
            edge = f32::NEG_INFINITY;
        }
        sites.push(q);
        boundaries.push(edge);
    }
    if sites.is_empty() {
        out.fill(1e12);
        return;
    }
    let mut k = 0;
    for (q, d) in out.iter_mut().enumerate() {
        while k + 1 < sites.len() && boundaries[k + 1] < q as f32 {
            k += 1;
        }
        *d = (q as f32 - sites[k] as f32).powi(2) + input[sites[k]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_text_coverage_table_matches_per_pixel_compositing() {
        let mut a = crate::annotation::Annotation::new(
            crate::annotation::Tool::Text,
            crate::annotation::Point { x: 0., y: 0. },
            crate::annotation::Style::default(),
        );
        a.update(crate::annotation::Point { x: 16., y: 16. });
        let mask: Vec<u8> = (0..=255).collect();
        for (color, opacity) in [(0x20252b, 1.), (0x315bb7, 0.6), (0xd9e2e9, 0.37)] {
            a.style.color = color;
            a.style.opacity = opacity;
            let fast = paint(&a, Rect::full(16, 16), &mask);
            let mut slow = crate::raster::empty(Rect::full(16, 16));
            for (p, &coverage) in slow.bgra.chunks_exact_mut(4).zip(&mask) {
                over(p, color, coverage as f32 / 255.);
                p[3] = (p[3] as f32 * opacity).round() as u8;
            }
            assert_eq!(fast.bgra, slow.bgra, "color={color:06x} opacity={opacity}");
        }
    }
    #[test]
    fn euclidean_outline_has_round_corners_and_preserves_fractional_coverage() {
        let mut mask = vec![0; 81];
        mask[40] = 255;
        let out = dilate(&mask, 9, 9, 2.);
        assert_eq!(out[40], 255);
        assert_eq!(out[42], 255);
        assert!(out[60] > 0 && out[60] < 80);
        assert_eq!(out[43], 0);
        mask[40] = 128;
        assert_eq!(dilate(&mask, 9, 9, 0.), mask);
    }
}
