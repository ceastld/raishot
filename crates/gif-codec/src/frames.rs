//! 原宿主索引差异与裁剪热循环；由同一优化 crate 编译，不改变输出矩形。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

/// 共用同一调色板的两帧索引变化包围盒；完全相同返回 None。
pub fn diff_bounds(previous: &[u8], current: &[u8], width: u32, height: u32) -> Option<Rect> {
    let row = width as usize;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..height as usize {
        let a = &previous[y * row..(y + 1) * row];
        let b = &current[y * row..(y + 1) * row];
        if a == b {
            continue;
        }
        let first = (0..width as usize).find(|x| a[*x] != b[*x]);
        let last = (0..width as usize).rev().find(|x| a[*x] != b[*x]);
        if let (Some(first), Some(last)) = (first, last) {
            min_x = min_x.min(first as u32);
            max_x = max_x.max(last as u32);
            min_y = min_y.min(y as u32);
            max_y = max_y.max(y as u32);
        }
    }
    (min_x != u32::MAX).then(|| Rect {
        left: min_x,
        top: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    })
}

pub fn crop(indices: &[u8], width: u32, rect: Rect) -> Vec<u8> {
    let row = width as usize;
    let mut out = Vec::with_capacity(rect.width as usize * rect.height as usize);
    for y in rect.top..rect.top + rect.height {
        let start = y as usize * row + rect.left as usize;
        out.extend_from_slice(&indices[start..start + rect.width as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(previous: &[u8], current: &[u8], width: u32, height: u32) {
        let actual = diff_bounds(previous, current, width, height);
        if let Some(actual) = actual {
            // 独立按全帧逐像素筛选，核对差异矩形包围所有且只到最外差异坐标。
            let changed: Vec<_> = previous
                .iter()
                .zip(current)
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .map(|(index, _)| (index as u32 % width, index as u32 / width))
                .collect();
            assert_eq!(actual.left, changed.iter().map(|(x, _)| *x).min().unwrap());
            assert_eq!(actual.top, changed.iter().map(|(_, y)| *y).min().unwrap());
            assert_eq!(
                actual.left + actual.width - 1,
                changed.iter().map(|(x, _)| *x).max().unwrap()
            );
            assert_eq!(
                actual.top + actual.height - 1,
                changed.iter().map(|(_, y)| *y).max().unwrap()
            );
        } else {
            assert_eq!(previous, current);
        }
    }

    #[test]
    fn gif_cached_frames_diff_and_crop_have_exact_single_pixel_and_full_frame_bounds() {
        for (width, height) in [(1, 1), (1, 7), (9, 1), (7, 5), (16, 8)] {
            let previous = vec![3; width as usize * height as usize];
            check(&previous, &previous, width, height);
            for point in 0..previous.len() {
                let mut current = previous.clone();
                current[point] = 241;
                check(&previous, &current, width, height);
                let rect = Rect {
                    left: point as u32 % width,
                    top: point as u32 / width,
                    width: 1,
                    height: 1,
                };
                assert_eq!(diff_bounds(&previous, &current, width, height), Some(rect));
                assert_eq!(crop(&current, width, rect), [241]);
            }
            let current = vec![17; previous.len()];
            check(&previous, &current, width, height);
            assert_eq!(
                diff_bounds(&previous, &current, width, height),
                Some(Rect {
                    left: 0,
                    top: 0,
                    width,
                    height
                })
            );
        }
    }

    #[test]
    fn gif_cached_frames_disjoint_changes_match_independent_pixel_bounds_oracle() {
        let (width, height) = (23, 17);
        let previous = vec![93; width * height];
        for phase in 0..13 {
            let mut current = previous.clone();
            for (index, pixel) in current.iter_mut().enumerate() {
                if (index * 7 + phase * 11) % 53 < 3 {
                    *pixel = 180;
                }
            }
            check(&previous, &current, width as u32, height as u32);
        }
    }

    #[test]
    fn gif_cached_frames_crop_matches_row_major_contents_at_every_origin() {
        let (width, height) = (7, 5);
        let frame: Vec<u8> = (0..width * height).map(|value| value as u8).collect();
        for top in 0..height {
            for left in 0..width {
                let rect = Rect {
                    left,
                    top,
                    width: width - left,
                    height: height - top,
                };
                let expected: Vec<_> = (top..height)
                    .flat_map(|y| {
                        let frame = &frame;
                        (left..width).map(move |x| frame[(y * width + x) as usize])
                    })
                    .collect();
                assert_eq!(crop(&frame, width, rect), expected);
            }
        }
    }
}
