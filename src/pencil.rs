//! 保角铅笔：原始样本只读，局部降噪、受限简化和贝塞尔拟合共用物理像素误差预算。
//! 不按线宽放大平滑范围，不做收笔二次润色。明显转角和首尾点必须精确经过。
use crate::{
    annotation::{distance_to_segment, Point},
    vector_path::{Span, VectorPath},
};

const ARM: f32 = 4.;
const FILTER_BUDGET: f32 = 0.75;
const SOURCE_BUDGET: f32 = 0.9;
const PATH_BUDGET: f32 = 0.915;
const FILTER_ARM: f32 = 3.;
const SIMPLIFY_BUDGET: f32 = 0.25;
const CURVE_BUDGET: f32 = 0.2;
const CHUNK: usize = 64;

pub fn fit(samples: &[Point]) -> VectorPath {
    let mut raw = Vec::with_capacity(samples.len());
    for &p in samples {
        if p.x.is_finite()
            && p.y.is_finite()
            && raw
                .last()
                .is_none_or(|last: &Point| last.distance(p) > 0.01)
        {
            raw.push(p);
        }
    }
    if raw.is_empty() {
        return VectorPath::default();
    }
    if raw.len() == 1 {
        return VectorPath {
            spans: vec![Span::Line {
                start: raw[0],
                end: raw[0],
            }],
            ..VectorPath::default()
        };
    }
    let distances = arc_lengths(&raw);
    let protected = corners(&raw, &distances);
    let mut fixed = protected.clone();
    for (i, &corner) in protected.iter().enumerate() {
        if corner && i > 0 && i + 1 < raw.len() {
            let left = distances.partition_point(|d| *d < distances[i] - FILTER_ARM);
            let right = distances.partition_point(|d| *d <= distances[i] + FILTER_ARM);
            fixed[left..right].fill(true);
        }
    }
    let mut filtered = raw.clone();
    // 两遍小范围滤波压低整数采样台阶；总位移按原始样本限制，不能累加两次误差预算。
    // 固定弧长位置使停顿或补点不会提高滤波强度；端点及明显转角邻域保持原位。
    let mut buffer = raw.clone();
    for _ in 0..2 {
        let previous = &filtered;
        for i in 1..raw.len() - 1 {
            if fixed[i] {
                continue;
            }
            let near_left = at_distance(previous, &distances, distances[i] - FILTER_ARM / 2.);
            let near_right = at_distance(previous, &distances, distances[i] + FILTER_ARM / 2.);
            let far_left = at_distance(previous, &distances, distances[i] - FILTER_ARM);
            let far_right = at_distance(previous, &distances, distances[i] + FILTER_ARM);
            let average = Point {
                x: (far_left.x
                    + 4. * near_left.x
                    + 6. * previous[i].x
                    + 4. * near_right.x
                    + far_right.x)
                    / 16.,
                y: (far_left.y
                    + 4. * near_left.y
                    + 6. * previous[i].y
                    + 4. * near_right.y
                    + far_right.y)
                    / 16.,
            };
            // 只消除法向抖动，不沿笔迹拉动样本；第二遍也锁定首尾及原始折点。
            let direction = unit(far_left, far_right);
            let normal = (average.x - previous[i].x) * -direction.y
                + (average.y - previous[i].y) * direction.x;
            let candidate = Point {
                x: previous[i].x - direction.y * normal,
                y: previous[i].y + direction.x * normal,
            };
            let delta = raw[i].distance(candidate);
            let mix = if delta > FILTER_BUDGET {
                FILTER_BUDGET / delta
            } else {
                1.
            };
            buffer[i] = lerp(raw[i], candidate, mix);
        }
        std::mem::swap(&mut filtered, &mut buffer);
    }
    let mut keep = protected.clone();
    // 有限样本块控制 RDP 最坏耗时；块边界只是结点，不被当作硬转角。
    let mut start = 0;
    while start < raw.len() - 1 {
        let end = (start + 1..raw.len())
            .find(|&i| protected[i] || i - start >= CHUNK)
            .unwrap_or(raw.len() - 1);
        simplify(&filtered, &raw, start, end, &mut keep);
        start = end;
    }
    let nodes: Vec<_> = keep
        .iter()
        .enumerate()
        .filter_map(|(i, k)| k.then_some(i))
        .collect();
    let mut spans = Vec::with_capacity(nodes.len() - 1);
    for (j, pair) in nodes.windows(2).enumerate() {
        let (i, k) = (pair[0], pair[1]);
        let (a, b) = (filtered[i], filtered[k]);
        let before = if j > 0 { filtered[nodes[j - 1]] } else { a };
        let after = nodes.get(j + 2).map_or(b, |&next| filtered[next]);
        let source_error = raw[i..=k]
            .iter()
            .map(|&p| distance_to_segment(p, a, b))
            .fold(0_f32, f32::max);
        // 三次曲线法向控制权重之和最多 3/4，留出原始样本误差与 0.03 px 细分预算。
        let curve_budget = ((PATH_BUDGET - source_error) / 0.75).clamp(0., CURVE_BUDGET);
        let c1 = if protected[i] {
            lerp(a, b, 1. / 3.)
        } else {
            tangent_control(a, tangent(before, a, b), a, b, true, curve_budget)
        };
        let c2 = if protected[k] {
            lerp(a, b, 2. / 3.)
        } else {
            tangent_control(b, tangent(a, b, after), a, b, false, curve_budget)
        };
        if distance_to_segment(c1, a, b) < 0.001 && distance_to_segment(c2, a, b) < 0.001 {
            spans.push(Span::Line { start: a, end: b });
        } else {
            spans.push(Span::Cubic {
                start: a,
                control1: c1,
                control2: c2,
                end: b,
            });
        }
    }
    VectorPath {
        spans,
        ..VectorPath::default()
    }
}
fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}
fn arc_lengths(p: &[Point]) -> Vec<f32> {
    let mut distance = vec![0.];
    for pair in p.windows(2) {
        distance.push(distance.last().unwrap() + pair[0].distance(pair[1]));
    }
    distance
}
fn at_distance(p: &[Point], d: &[f32], target: f32) -> Point {
    if target <= 0. {
        return p[0];
    }
    let i = d.partition_point(|&v| v < target).min(p.len() - 1);
    if i == 0 {
        return p[0];
    }
    lerp(
        p[i - 1],
        p[i],
        ((target - d[i - 1]) / (d[i] - d[i - 1])).clamp(0., 1.),
    )
}
fn signed_turn(a: Point, b: Point, c: Point) -> f32 {
    let length = a.distance(b) * b.distance(c);
    if length <= 0.0001 {
        return 0.;
    }
    let dot = (b.x - a.x) * (c.x - b.x) + (b.y - a.y) * (c.y - b.y);
    let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
    cross.atan2(dot).to_degrees()
}
fn corners(p: &[Point], d: &[f32]) -> Vec<bool> {
    let mut scores = vec![0_f32; p.len()];
    let mut hard = vec![false; p.len()];
    for i in 1..p.len() - 1 {
        let local_turn = signed_turn(p[i - 1], p[i], p[i + 1]);
        let local = local_turn.abs();
        if p.len() == 3 && local >= 40. {
            scores[i] = local * local;
            continue;
        }
        if local < 35. {
            continue;
        }
        // 一像素量化台阶常有 90° 局部转向，但两侧不足 2 px 就会转回。
        // 转角必须在短臂上仍有方向支持，不能仅凭相邻三个整数点锁死。
        let supported = signed_turn(
            at_distance(p, d, d[i] - 2.),
            p[i],
            at_distance(p, d, d[i] + 2.),
        );
        if supported.abs() < local * 0.75
            || (supported * local_turn < 0. && supported.abs().min(local) < 150.)
        {
            continue;
        }
        let left = at_distance(p, d, d[i] - ARM);
        let right = at_distance(p, d, d[i] + ARM);
        let broad_turn = signed_turn(left, p[i], right);
        let broad = broad_turn.abs();
        // 细碎抖动的局部转向可能与整体弯曲相反，不能把它锁成不可平滑的折点。
        // 接近折返时 +180° 与 -180° 表示同一方向，微小采样误差可能跨过符号边界。
        if local_turn * broad_turn < 0. && local.min(broad) < 150. {
            continue;
        }
        if broad >= 40. && left.distance(p[i]) >= 2. && right.distance(p[i]) >= 2. {
            scores[i] = broad * local;
            // 明确折角逐个保护：台阶或回折的两个直角即使相距不足 3 px 也不能合并。
            // 温和的连续转弯仍在下方只保护邻域最明显的一点，避免锁住像素噪声。
            hard[i] = local >= 75.;
        }
    }
    let mut result = vec![false; p.len()];
    result[0] = true;
    result[p.len() - 1] = true;
    for i in 1..p.len() - 1 {
        if scores[i] == 0. {
            continue;
        }
        let left = d.partition_point(|v| *v < d[i] - 3.);
        let right = d.partition_point(|v| *v <= d[i] + 3.);
        result[i] = hard[i]
            || !(left..right)
                .any(|j| j != i && (scores[j] > scores[i] || scores[j] == scores[i] && j < i));
    }
    result
}
fn simplify(p: &[Point], raw: &[Point], start: usize, end: usize, keep: &mut [bool]) {
    keep[start] = true;
    keep[end] = true;
    let mut pending = vec![(start, end)];
    while let Some((a, b)) = pending.pop() {
        if b <= a + 1 {
            continue;
        }
        let (index, error) = (a + 1..b)
            .map(|i| {
                (
                    i,
                    (distance_to_segment(p[i], p[a], p[b]) / SIMPLIFY_BUDGET)
                        .max(distance_to_segment(raw[i], p[a], p[b]) / SOURCE_BUDGET),
                )
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        if error > 1. {
            keep[index] = true;
            pending.push((a, index));
            pending.push((index, b));
        }
    }
}
fn unit(a: Point, b: Point) -> Point {
    let length = a.distance(b);
    if length < 0.0001 {
        return Point { x: 0., y: 0. };
    }
    Point {
        x: (b.x - a.x) / length,
        y: (b.y - a.y) / length,
    }
}
fn tangent(before: Point, anchor: Point, after: Point) -> Point {
    let incoming = unit(before, anchor);
    let outgoing = unit(anchor, after);
    unit(
        Point { x: 0., y: 0. },
        Point {
            x: incoming.x + outgoing.x,
            y: incoming.y + outgoing.y,
        },
    )
}
fn tangent_control(
    anchor: Point,
    direction: Point,
    a: Point,
    b: Point,
    start: bool,
    budget: f32,
) -> Point {
    let chord = unit(a, b);
    let normal = (direction.x * -chord.y + direction.y * chord.x).abs();
    // 限制控制柄长度，而不是分别截断 x/y 或法向分量；相邻曲线仍共享同一切线方向。
    let forward = direction.x * chord.x + direction.y * chord.y;
    let handle = if forward <= 0. {
        0.
    } else {
        (a.distance(b) / 3.).min(budget / normal.max(0.0001))
    } * if start { 1. } else { -1. };
    Point {
        x: anchor.x + direction.x * handle,
        y: anchor.y + direction.y * handle,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn p(x: f32, y: f32) -> Point {
        Point { x, y }
    }
    fn distance(p: Point, segments: &[(Point, Point)]) -> f32 {
        segments
            .iter()
            .map(|&(a, b)| distance_to_segment(p, a, b))
            .fold(f32::INFINITY, f32::min)
    }
    #[test]
    fn ninety_degree_v_and_reversal_keep_exact_corner_and_endpoints() {
        for raw in [
            vec![p(10., 10.), p(100., 10.), p(100., 80.)],
            vec![p(10., 10.), p(60., 90.), p(110., 10.)],
            vec![p(10., 10.), p(100., 10.), p(30., 10.)],
        ] {
            let path = fit(&raw);
            assert_eq!(path.spans.first().unwrap().start(), raw[0]);
            assert_eq!(path.spans.last().unwrap().end(), *raw.last().unwrap());
            assert!(path.spans.iter().any(|s| s.end() == raw[1]));
            assert!(path.spans.iter().all(|s| matches!(s, Span::Line { .. })));
        }
    }
    #[test]
    fn dense_corner_survives_and_neighbouring_edges_remain_straight() {
        let raw: Vec<_> = (0..=80)
            .map(|x| p(x as f32, 0.))
            .chain((1..=80).map(|y| p(80., y as f32)))
            .collect();
        let path = fit(&raw);
        assert!(path.spans.iter().any(|s| s.end() == p(80., 0.)));
        assert!(path
            .flattened()
            .iter()
            .all(|&(a, b)| (a.y == 0. && b.y == 0.) || (a.x == 80. && b.x == 80.)));
    }
    #[test]
    fn nearby_opposite_corners_keep_both_vertices_at_any_sampling_density() {
        // 两像素高的台阶仍是两个独立折角，不能用一次邻域去重把其中一个磨圆。
        let sparse = vec![p(0., 0.), p(20., 0.), p(20., 2.), p(40., 2.)];
        let dense: Vec<_> = (0..=20)
            .map(|x| p(x as f32, 0.))
            .chain((1..=2).map(|y| p(20., y as f32)))
            .chain((21..=40).map(|x| p(x as f32, 2.)))
            .collect();
        for raw in [sparse, dense] {
            let path = fit(&raw);
            for vertex in [p(20., 0.), p(20., 2.)] {
                assert!(
                    path.spans.iter().any(|s| s.end() == vertex),
                    "lost deliberate step vertex {vertex:?}: {:?}",
                    path.spans
                );
            }
            assert!(path
                .flattened()
                .iter()
                .all(|&(a, b)| (a.y == 0. && b.y == 0.)
                    || (a.x == 20. && b.x == 20.)
                    || (a.y == 2. && b.y == 2.)));
        }
    }
    #[test]
    fn nearby_same_direction_corners_keep_a_narrow_hairpin() {
        let raw = [p(0., 0.), p(20., 0.), p(20., 2.), p(0., 2.)];
        let path = fit(&raw);
        assert_eq!(path.spans, VectorPath::from_polyline(&raw).spans);
        let reversed: Vec<_> = raw.iter().copied().rev().collect();
        assert_eq!(
            fit(&reversed).spans,
            VectorPath::from_polyline(&reversed).spans
        );
    }
    #[test]
    fn near_reversal_survives_signed_angle_wrap_and_subpixel_noise() {
        let raw = [
            p(0., 0.),
            p(99., 0.),
            p(100., 0.),
            p(99.5, 0.001),
            p(0., -0.3),
        ];
        let path = fit(&raw);
        assert!(
            path.spans.iter().any(|s| s.end() == raw[2]),
            "reversal tip moved"
        );
        assert_eq!(path.spans.first().unwrap().start(), raw[0]);
        assert_eq!(path.spans.last().unwrap().end(), raw[4]);
        for point in raw {
            assert!(distance(point, &path.flattened()) < 0.95);
        }
    }
    #[test]
    fn noise_is_reduced_without_moving_raw_samples_or_ends() {
        let raw: Vec<_> = (0..160)
            .map(|i| p(i as f32, if i % 2 == 0 { 0.8 } else { -0.8 }))
            .collect();
        let original = raw.clone();
        let path = fit(&raw);
        let segments = path.flattened();
        let rough: f32 = raw[3..157].iter().map(|p| p.y.abs()).sum();
        let clean: f32 = (3..157)
            .map(|i| {
                let x = i as f32;
                segments
                    .iter()
                    .filter(|(a, b)| a.x <= x && b.x >= x)
                    .map(|(a, b)| lerp(*a, *b, (x - a.x) / (b.x - a.x)).y.abs())
                    .fold(f32::INFINITY, f32::min)
            })
            .sum();
        assert!(clean < rough * 0.75, "clean={clean}, rough={rough}");
        assert_eq!(raw, original);
        assert_eq!(path.spans[0].start(), raw[0]);
        assert_eq!(path.spans.last().unwrap().end(), *raw.last().unwrap());
        for point in raw {
            assert!(distance(point, &segments) < 0.95);
        }
    }
    #[test]
    fn deliberate_curve_has_cubics_and_stays_near_input() {
        let raw: Vec<_> = (0..240)
            .map(|i| {
                let t = i as f32 / 239. * std::f32::consts::PI;
                p(100. + 80. * t.cos(), 100. + 80. * t.sin())
            })
            .collect();
        let path = fit(&raw);
        assert!(path.spans.iter().any(|s| matches!(s, Span::Cubic { .. })));
        let lines = path.flattened();
        for point in &raw {
            assert!(distance(*point, &lines) < 0.95);
        }
        let original: Vec<_> = raw.windows(2).map(|p| (p[0], p[1])).collect();
        for (a, b) in lines {
            assert!(distance(lerp(a, b, 0.5), &original) < 0.95);
        }
        assert!(path.spans.len() < raw.len() / 2);
        assert!(path.svg_commands().contains('C'));
    }
    #[test]
    fn empty_dot_duplicate_and_small_samples_are_finite() {
        assert!(fit(&[]).spans.is_empty());
        let raw = [p(3., 3.), p(3., 3.), p(f32::NAN, 0.), p(3., 3.)];
        assert_eq!(fit(&raw).flattened(), vec![(p(3., 3.), p(3., 3.))]);
        let tiny = [p(1., 1.), p(1.1, 1.2), p(1.2, 1.3)];
        let path = fit(&tiny);
        assert_eq!(path.spans.last().unwrap().end(), tiny[2]);
        assert!(path
            .flattened()
            .iter()
            .all(|(a, b)| a.x.is_finite() && b.y.is_finite()));
    }
    fn at_x(segments: &[(Point, Point)], x: f32) -> f32 {
        segments
            .iter()
            .find(|(a, b)| a.x <= x && b.x >= x)
            .map(|(a, b)| lerp(*a, *b, (x - a.x) / (b.x - a.x)).y)
            .unwrap()
    }
    fn direction(span: Span, start: bool) -> Point {
        match span {
            Span::Line { start, end } => unit(start, end),
            Span::Cubic {
                start: a,
                control1,
                control2,
                end,
            } => {
                if start {
                    unit(a, control1)
                } else {
                    unit(control2, end)
                }
            }
        }
    }
    #[test]
    fn digitized_diagonal_loses_stair_steps_without_drifting_from_samples() {
        let raw: Vec<_> = (0..220)
            .map(|i| p(i as f32, (i as f32 * 0.37).round()))
            .collect();
        let path = fit(&raw);
        let lines = path.flattened();
        let rough: f32 = raw[8..212].iter().map(|p| (p.y - p.x * 0.37).powi(2)).sum();
        let clean: f32 = (8..212)
            .map(|i| (at_x(&lines, i as f32) - i as f32 * 0.37).powi(2))
            .sum();
        assert!(clean < rough * 0.45, "rough={rough}, clean={clean}");
        for point in raw {
            assert!(distance(point, &lines) < 1.05);
        }
        assert!(path.spans.len() < 60, "{} spans", path.spans.len());
    }
    #[test]
    fn smooth_nodes_share_tangents_instead_of_clipping_them_per_span() {
        let raw: Vec<_> = (0..240)
            .map(|i| {
                let t = i as f32 / 239. * std::f32::consts::PI;
                p(100. + 80. * t.cos(), 100. + 80. * t.sin())
            })
            .collect();
        let path = fit(&raw);
        for spans in path.spans.windows(2) {
            let a = direction(spans[0], false);
            let b = direction(spans[1], true);
            assert!(
                (a.x * b.y - a.y * b.x).abs() < 0.002,
                "mismatched tangents {a:?} {b:?}"
            );
            assert!(a.x * b.x + a.y * b.y > 0.99);
        }
    }
    #[test]
    fn arc_length_filter_is_stable_when_the_same_trace_has_extra_samples() {
        let raw: Vec<_> = (0..160)
            .map(|i| p(i as f32, (i as f32 * 0.37).round()))
            .collect();
        let mut dense = Vec::new();
        for pair in raw.windows(2) {
            for i in 0..4 {
                dense.push(lerp(pair[0], pair[1], i as f32 / 4.));
            }
        }
        dense.push(*raw.last().unwrap());
        let sparse = fit(&raw).flattened();
        let frequent = fit(&dense).flattened();
        let difference: f32 = (8..152)
            .map(|i| (at_x(&sparse, i as f32) - at_x(&frequent, i as f32)).abs())
            .sum();
        assert!(
            difference / 144. < 0.12,
            "density changed shape by {} px",
            difference / 144.
        );
    }
    #[test]
    fn alternating_jitter_reduces_to_a_small_number_of_smooth_spans() {
        let raw: Vec<_> = (0..400)
            .map(|i| p(i as f32 * 0.8, if i % 2 == 0 { 0.8 } else { -0.8 }))
            .collect();
        let path = fit(&raw);
        let lines = path.flattened();
        assert!(
            path.spans.len() < 50,
            "{} spans for a nearly straight line",
            path.spans.len()
        );
        let mean: f32 = (8..310).map(|x| at_x(&lines, x as f32).abs()).sum::<f32>() / 302.;
        assert!(mean < 0.2, "residual jitter={mean}");
        assert_eq!(path.spans.first().unwrap().start(), raw[0]);
        assert_eq!(path.spans.last().unwrap().end(), *raw.last().unwrap());
    }
    #[test]
    fn digitized_slopes_and_pixel_phases_have_subpixel_residual_error() {
        let mut total_error = 0.;
        let mut count = 0;
        for slope in [0.08_f32, 0.18, 0.27, 0.37, 0.68, 0.95] {
            for sign in [-1., 1.] {
                for phase in [0., 0.3, 0.7] {
                    let slope = slope * sign;
                    let raw: Vec<_> = (0..320)
                        .map(|i| p(i as f32, (i as f32 * slope + phase).round()))
                        .collect();
                    let path = fit(&raw);
                    assert_eq!(path.spans.first().unwrap().start(), raw[0]);
                    assert_eq!(path.spans.last().unwrap().end(), *raw.last().unwrap());
                    let lines = path.flattened();
                    let mean_square = (8..312)
                        .map(|i| {
                            let x = i as f32;
                            ((at_x(&lines, x) - (x * slope + phase)) / (1. + slope * slope).sqrt())
                                .powi(2)
                        })
                        .sum::<f32>()
                        / 304.;
                    assert!(
                        mean_square.sqrt() < 0.18,
                        "slope={slope}, phase={phase}, rms={}",
                        mean_square.sqrt()
                    );
                    total_error += mean_square;
                    count += 1;
                }
            }
        }
        assert!(
            (total_error / count as f32).sqrt() < 0.105,
            "aggregate rms={}",
            (total_error / count as f32).sqrt()
        );
    }
    #[test]
    fn quantized_small_ovals_do_not_turn_single_pixel_steps_into_corners() {
        for (rx, ry) in [(15., 30.), (12., 36.), (30., 15.)] {
            for phase in [0., 0.3, 0.7] {
                let raw: Vec<_> = (0..=300)
                    .map(|i| {
                        let t = i as f32 / 300. * std::f32::consts::TAU;
                        p(
                            (100. + rx * t.cos() + phase).round(),
                            (100. + ry * t.sin() + phase).round(),
                        )
                    })
                    .collect();
                let path = fit(&raw);
                for pair in path.spans.windows(2) {
                    let a = direction(pair[0], false);
                    let b = direction(pair[1], true);
                    let cosine = a.x * b.x + a.y * b.y;
                    assert!(
                        cosine > 0.996,
                        "false corner on oval ({rx}, {ry}), phase={phase}, point={:?}, angle={}",
                        pair[0].end(),
                        cosine.clamp(-1., 1.).acos().to_degrees()
                    );
                }
                let lines = path.flattened();
                for point in raw {
                    assert!(
                        distance(point, &lines) < 0.95,
                        "oval smoothing exceeded source budget"
                    );
                }
            }
        }
    }
    #[test]
    fn two_pass_filter_preserves_error_budget_on_small_digitized_curves() {
        for radius in [3., 8., 20., 80.] {
            let raw: Vec<_> = (0..240)
                .map(|i| {
                    let t = i as f32 / 239. * std::f32::consts::PI * 1.5;
                    p(
                        (100. + radius * t.cos()).round(),
                        (100. + radius * t.sin()).round(),
                    )
                })
                .collect();
            let lines = fit(&raw).flattened();
            for &point in &raw {
                assert!(
                    distance(point, &lines) < 0.95,
                    "curve drifted: radius={radius}, point={point:?}"
                );
            }
            let source: Vec<_> = raw.windows(2).map(|p| (p[0], p[1])).collect();
            for (a, b) in lines {
                assert!(
                    distance(lerp(a, b, 0.5), &source) < 0.95,
                    "curve overshot: radius={radius}"
                );
            }
        }
    }
}
