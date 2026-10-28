//! 仅在原图笔画更支持共同水平边缘时拉平轻斜检测框；真实斜字仍沿原投影。
use super::*;

const STRIPS: usize = 8;
const EDGE_BAND: f64 = 0.6;
// 斜边经整数像素取整会形成长平台；原斜角允许一像素量化不确定度，
// 避免窄带计票反而让真正浅斜的笔画输给水平平台。
const TILTED_EDGE_BAND: f64 = 1.;

#[derive(Clone, Copy, Default)]
struct Column {
    top: usize,
    bottom: usize,
}

fn peak_count(mut residuals: Vec<f64>, band: f64) -> usize {
    residuals.sort_unstable_by(f64::total_cmp);
    let (mut left, mut best) = (0, 0);
    for right in 0..residuals.len() {
        while residuals[right] - residuals[left] > band {
            left += 1;
        }
        best = best.max(right - left + 1);
    }
    best
}

/// 同一条边缘更支持水平而不是原斜角，且票数不能集中在单个字或局部平台。
fn horizontal_edge(
    samples: &[(usize, usize)],
    slope: f64,
    strong_left: usize,
    strong_right: usize,
) -> bool {
    let tilted = peak_count(
        samples
            .iter()
            .map(|&(x, y)| y as f64 - slope * (x as f64 + 0.5))
            .collect(),
        TILTED_EDGE_BAND,
    );
    let mut ordered = samples.to_vec();
    ordered.sort_unstable_by_key(|&(_, value)| value);
    // 屏幕上沿/下沿是整数像素边缘，0.6px带内只会有同一整数高度。
    let horizontal = peak_count(ordered.iter().map(|&(_, y)| y as f64).collect(), EDGE_BAND);
    if horizontal < 8 || horizontal * 100 < samples.len() * 35 || horizontal * 4 < tilted * 5 {
        return false;
    }
    let width = strong_right - strong_left;
    let mut start = 0;
    while start < ordered.len() {
        let mut end = start + 1;
        while end < ordered.len() && ordered[end].1 == ordered[start].1 {
            end += 1;
        }
        let group = &ordered[start..end];
        if group.len() == horizontal {
            let left = group.iter().map(|&(x, _)| x).min().unwrap();
            let right = group.iter().map(|&(x, _)| x + 1).max().unwrap();
            let mut occupied = [false; STRIPS];
            for &(x, _) in group {
                let index = ((2 * (x - strong_left) + 1) * STRIPS / (2 * width)).min(STRIPS - 1);
                occupied[index] = true;
            }
            if (right - left) as f64 >= width as f64 * 0.7
                && occupied.iter().filter(|&&value| value).count() >= 6
            {
                return true;
            }
        }
        start = end;
    }
    false
}

/// 调用者已经核实完整背景、单一极性和单带；本门只证明屏幕坐标中的水平性。
pub(super) fn horizontal_geometry(
    projection: Projection,
    roi: Roi,
    rgb: &RgbImage,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<TextGeometry>> {
    checkpoint()?;
    let q = projection.quad();
    let start = projection.point(0., 0.5);
    let end = projection.point(1., 0.5);
    let width = f64::from((end.x - start.x).abs());
    let rise = f64::from((end.y - start.y).abs());
    let slope = rise / width;
    let signed_slope = f64::from(end.y - start.y) / f64::from(end.x - start.x);
    // 只修正有可观察斜升的近水平仿射框；透视四边形不作此推断。
    let affine_error = f64::from(q[0].x + q[2].x - q[1].x - q[3].x)
        .hypot(f64::from(q[0].y + q[2].y - q[1].y - q[3].y));
    if !width.is_finite()
        || width <= 0.
        || rise < 1.5
        || !(0.002_617_999..=0.052_407_78).contains(&slope)
        || affine_error > 0.05
    {
        return Ok(None);
    }
    let (left, right) = (f64::from(start.x.min(end.x)), f64::from(start.x.max(end.x)));
    let background = rgb.pixels[roi.top * rgb.width + roi.left];
    let mut columns = vec![Column::default(); roi.right - roi.left];
    let (mut support_top, mut support_bottom) = (usize::MAX, 0usize);
    let (mut strong_left, mut strong_right) = (usize::MAX, 0usize);
    let (mut strong_top, mut strong_bottom) = (usize::MAX, 0usize);
    let mut strong_count = 0usize;
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            let pixel = rgb.pixels[y * rgb.width + x];
            if pixel == background {
                continue;
            }
            // 淡 AA 也必须以完整像素单元保留，不能拿像素中心代替足迹。
            if (x as f64) < left || (x + 1) as f64 > right {
                return Ok(None);
            }
            support_top = support_top.min(y);
            support_bottom = support_bottom.max(y + 1);
            let delta = (0..3).map(|c| pixel[c] - background[c]).sum::<f32>() / 3.;
            if delta.abs() < STRONG_DELTA {
                continue;
            }
            strong_count += 1;
            strong_left = strong_left.min(x);
            strong_right = strong_right.max(x + 1);
            strong_top = strong_top.min(y);
            strong_bottom = strong_bottom.max(y + 1);
            let column = &mut columns[x - roi.left];
            if column.bottom == 0 {
                column.top = y;
            }
            column.top = column.top.min(y);
            column.bottom = column.bottom.max(y + 1);
        }
    }
    // 用实际强墨迹高度而不是宽松检测框高判断长宽比，单字和稀疏噪点回退。
    if strong_count < 64
        || strong_bottom < strong_top.saturating_add(6)
        || strong_right < strong_left
        || ((strong_right - strong_left) as f64) < width * 0.7
        || width < (strong_bottom - strong_top) as f64 * 2.5
    {
        return Ok(None);
    }
    let top: Vec<_> = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.bottom > 0)
        .map(|(i, column)| (i + roi.left, column.top))
        .collect();
    let bottom: Vec<_> = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.bottom > 0)
        .map(|(i, column)| (i + roi.left, column.bottom))
        .collect();
    checkpoint()?;
    if !horizontal_edge(&bottom, signed_slope, strong_left, strong_right)
        && !horizontal_edge(&top, signed_slope, strong_left, strong_right)
    {
        return Ok(None);
    }
    // 与普通行选区使用相同的字号留白，拉平不能重新退回贴笔画的窄框。
    let padding = selection_padding((support_bottom - support_top) as f64) as f32;
    let top_margin = ((support_top.saturating_sub(roi.top + 1)) as f32 * 0.2).max(padding);
    let bottom_margin = ((roi.bottom.saturating_sub(support_bottom + 1)) as f32 * 0.2).max(padding);
    let top = support_top as f32 - top_margin;
    let bottom = support_bottom as f32 + bottom_margin;
    let center = (top + bottom) * 0.5;
    let half_height = ((bottom - top).max(normal_height(projection) as f32 * 0.6)) * 0.5;
    let top = center - half_height;
    let bottom = center + half_height;
    // 没有检查过的外围不能为凑留白而纳入选区。
    if top < (roi.top + 1) as f32 || bottom > (roi.bottom - 1) as f32 {
        return Ok(None);
    }
    let (first, last) = if start.x < end.x {
        (top, bottom)
    } else {
        (bottom, top)
    };
    let geometry = TextGeometry {
        quad: [
            Point {
                x: start.x,
                y: first,
            },
            Point { x: end.x, y: first },
            Point { x: end.x, y: last },
            Point {
                x: start.x,
                y: last,
            },
        ],
    };
    let Some(candidate) = geometry.validated_in(Rect::full(rgb.width as u32, rgb.height as u32))
    else {
        return Ok(None);
    };
    // f32 成品框必须再次包住全部非背景像素单元，而非只通过强像素基线。
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            if rgb.pixels[y * rgb.width + x] == background {
                continue;
            }
            let Some([u0, v0, u1, v1]) = cell(candidate, x, y) else {
                return Ok(None);
            };
            if u0 < 0. || v0 < 0. || u1 > 1. || v1 > 1. {
                return Ok(None);
            }
        }
    }
    checkpoint()?;
    Ok(Some(geometry))
}

#[cfg(test)]
#[path = "ink_alignment_tests.rs"]
mod tests;
