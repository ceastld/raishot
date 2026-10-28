//! 截图 OCR 结果面板的纯模型：停靠布局、识别行／段落划分与图上命中。
//! 布局输入输出都是窗口逻辑坐标（DIP）；行范围是原文 UTF-8 byte 范围。
use crate::{annotation::Point, toolbar_layout::PanelRect};
use std::ops::Range;

pub(crate) const PANEL_WIDTH: f32 = 340.;
const PANEL_GAP: f32 = 12.;
const EDGE: f32 = 8.;
const MIN_HEIGHT: f32 = 360.;
/// 标题行与分段切换的固定高度；正文区和底栏按此切分。
pub(crate) const HEADER_HEIGHT: f32 = 44.;
pub(crate) const FOOTER_HEIGHT: f32 = 52.;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PanelView {
    #[default]
    Lines,
    Paragraphs,
    Table,
    /// 译文独立一页；进入时自动翻译全文。
    Translation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelSide {
    Right,
    Left,
    /// 两侧都放不下时贴显示器右缘，覆盖选区右部。
    Inside,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelLayout {
    pub panel: PanelRect,
    pub side: PanelSide,
}

impl PanelLayout {
    /// 标题以下的整块区域；敏感文字等临时内容覆盖在这里。
    pub(crate) fn sheet(self) -> PanelRect {
        let top = self.panel.y + HEADER_HEIGHT;
        PanelRect {
            x: self.panel.x + EDGE,
            y: top,
            width: (self.panel.width - EDGE * 2.).max(1.),
            height: (self.panel.bottom() - EDGE - top).max(1.),
        }
    }

    /// 底栏上沿；底部浮层以此对齐。
    pub(crate) fn footer_top(self) -> f32 {
        self.panel.bottom() - FOOTER_HEIGHT
    }

    pub(crate) fn contains(self, x: f32, y: f32) -> bool {
        let p = self.panel;
        x >= p.x && x < p.right() && y >= p.y && y < p.bottom()
    }
}

/// 面板停在选区右侧，上沿对齐选区，下沿对齐选区下方的工具栏；工具栏比选区宽而
/// 伸到面板位置时，面板依次改为止于工具栏上方、移到工具栏外侧，再换到左侧；
/// 两侧都放不下时贴显示器右缘。只有所有候选都放不下时才接受与工具栏重叠。
pub(crate) fn panel_layout(
    monitor: PanelRect,
    selection: PanelRect,
    toolbar: Option<PanelRect>,
) -> PanelLayout {
    let width = PANEL_WIDTH.min(monitor.width - EDGE * 2.).max(1.);
    let toolbar = toolbar.filter(|t| valid(*t));
    let max_height = (monitor.height - EDGE * 2.).max(1.);
    let low = monitor.y + EDGE;
    let fits_x = |x: f32| x >= monitor.x + EDGE && x + width <= monitor.right() - EDGE;
    // 从 top 到 bottom 的面板，不足最小高度时向下延伸并留在显示器内。
    let place = |x: f32, bottom: f32, min_height: f32| {
        let height = (bottom - selection.y).max(min_height).min(max_height);
        let high = (monitor.bottom() - EDGE - height).max(low);
        PanelRect {
            x,
            y: selection.y.clamp(low, high),
            width,
            height,
        }
    };
    let clear = |rect: PanelRect| toolbar.is_none_or(|t| !overlaps(rect, t));
    // 完整高度：与选区下方的工具栏下沿齐平。
    let full_bottom = toolbar
        .filter(|t| t.y >= selection.y)
        .map_or(selection.bottom(), |t| selection.bottom().max(t.bottom()));
    // 缩短：止于工具栏上沿之上，高度仍需达到最小高度。
    let short = |x: f32| {
        let t = toolbar.filter(|t| t.y > selection.y)?;
        let bottom = t.y - EDGE;
        (bottom - selection.y >= MIN_HEIGHT).then(|| place(x, bottom, MIN_HEIGHT))
    };
    let right = selection.right() + PANEL_GAP;
    let left = selection.x - PANEL_GAP - width;
    let outside_right = toolbar.map(|t| t.right().max(selection.right()) + PANEL_GAP);
    let outside_left = toolbar.map(|t| t.x.min(selection.x) - PANEL_GAP - width);
    let sides = [
        (PanelSide::Right, Some(right), outside_right),
        (PanelSide::Left, Some(left), outside_left),
    ];
    for (side, beside, outside) in sides {
        let Some(x) = beside.filter(|x| fits_x(*x)) else {
            continue;
        };
        let full = place(x, full_bottom, MIN_HEIGHT);
        if clear(full) {
            return PanelLayout { panel: full, side };
        }
        if let Some(panel) = short(x).filter(|panel| clear(*panel)) {
            return PanelLayout { panel, side };
        }
        if let Some(x) = outside.filter(|x| fits_x(*x)) {
            let panel = place(x, selection.bottom(), MIN_HEIGHT);
            if clear(panel) {
                return PanelLayout { panel, side };
            }
        }
    }
    let x = (monitor.right() - EDGE - width).max(monitor.x);
    let inside = place(x, full_bottom, MIN_HEIGHT);
    let panel = if clear(inside) {
        inside
    } else {
        // 覆盖选区时也优先让出工具栏：缩到工具栏上方，或放到工具栏下方。
        toolbar
            .and_then(|t| {
                let above = (t.y - EDGE - low >= MIN_HEIGHT / 2.).then(|| PanelRect {
                    y: low,
                    height: (t.y - EDGE - low).min(max_height),
                    ..inside
                });
                let below_top = t.bottom() + EDGE;
                let below_height = monitor.bottom() - EDGE - below_top;
                let below = (below_height >= MIN_HEIGHT / 2.).then_some(PanelRect {
                    y: below_top,
                    height: below_height,
                    ..inside
                });
                above
                    .into_iter()
                    .chain(below)
                    .max_by(|a, b| a.height.total_cmp(&b.height))
            })
            .unwrap_or(inside)
    };
    PanelLayout {
        panel,
        side: PanelSide::Inside,
    }
}

fn overlaps(a: PanelRect, b: PanelRect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// 选区旁标记条：完整容纳时优先上方、再下方；无空位交结果面板回退。
#[cfg(test)]
pub(crate) fn mark_bar_origin(
    monitor: PanelRect,
    selected: PanelRect,
    width: f32,
    height: f32,
) -> Option<(f32, f32)> {
    mark_bar_origin_avoiding(monitor, selected, width, height, &[])
}

/// 选字栏和面板回退共用同一判定；障碍与选字范围均已投影到客户区 DIP。
pub(crate) fn mark_bar_origin_avoiding(
    monitor: PanelRect,
    selected: PanelRect,
    width: f32,
    height: f32,
    obstacles: &[PanelRect],
) -> Option<(f32, f32)> {
    if !valid(monitor)
        || !valid(selected)
        || [
            monitor.right(),
            monitor.bottom(),
            selected.right(),
            selected.bottom(),
            width,
            height,
        ]
        .iter()
        .any(|value| !value.is_finite())
        || width <= 0.
        || height <= 0.
    {
        return None;
    }
    let area = PanelRect {
        x: monitor.x + EDGE,
        y: monitor.y + EDGE,
        width: monitor.width - EDGE * 2.,
        height: monitor.height - EDGE * 2.,
    };
    place_mark_bar(
        area,
        selected,
        (width, height),
        selected.x,
        false,
        obstacles,
    )
}

/// 在完整可见区内寻找一行栏；沿障碍边缘寻找位移最小的位置，等距保留原上下偏好。
/// 只允许在整组文字的上方或下方，绝不把栏夹回文字内或裁掉可点击控件。
pub(crate) fn place_mark_bar(
    area: PanelRect,
    selected: PanelRect,
    dimensions: (f32, f32),
    preferred_x: f32,
    prefer_below: bool,
    obstacles: &[PanelRect],
) -> Option<(f32, f32)> {
    let (width, height) = dimensions;
    let finite_rect = |r: PanelRect| valid(r) && r.right().is_finite() && r.bottom().is_finite();
    if !finite_rect(area)
        || !finite_rect(selected)
        || !overlaps(area, selected)
        || !width.is_finite()
        || !height.is_finite()
        || !preferred_x.is_finite()
        || width <= 0.
        || height <= 0.
        || width > area.width
        || height > area.height
        || obstacles.iter().any(|r| !finite_rect(*r))
    {
        return None;
    }
    let gap = 8_f64;
    let (left, top) = (f64::from(area.x), f64::from(area.y));
    let (right, bottom) = (left + f64::from(area.width), top + f64::from(area.height));
    let (width, height) = (f64::from(width), f64::from(height));
    let preferred_x = f64::from(preferred_x).clamp(left, right - width);
    let mut xs = vec![preferred_x, left, right - width];
    for obstacle in obstacles {
        xs.extend([
            f64::from(obstacle.x) - gap - width,
            f64::from(obstacle.x) + f64::from(obstacle.width) + gap,
        ]);
    }
    xs.retain(|x| x.is_finite() && *x >= left && *x + width <= right);
    xs.sort_by(|a, b| {
        (a - preferred_x)
            .abs()
            .total_cmp(&(b - preferred_x).abs())
            .then_with(|| a.total_cmp(b))
    });
    xs.dedup();
    let selected_top = f64::from(selected.y);
    let selected_bottom = selected_top + f64::from(selected.height);
    let preferred_y = if prefer_below {
        selected_bottom + gap
    } else {
        selected_top - gap - height
    };
    let mut best: Option<(f64, (f32, f32))> = None;
    for below in [prefer_below, !prefer_below] {
        let boundary = if below {
            selected_bottom + gap
        } else {
            selected_top - gap - height
        };
        let mut ys = vec![boundary, top, bottom - height];
        for obstacle in obstacles {
            ys.extend([
                f64::from(obstacle.y) - gap - height,
                f64::from(obstacle.y) + f64::from(obstacle.height) + gap,
            ]);
        }
        ys.retain(|y| {
            y.is_finite()
                && *y >= top
                && *y + height <= bottom
                && if below {
                    *y >= boundary
                } else {
                    *y <= boundary
                }
        });
        ys.sort_by(|a, b| {
            if below {
                a.total_cmp(b)
            } else {
                b.total_cmp(a)
            }
        });
        ys.dedup();
        for y in ys {
            for x in &xs {
                let clear = obstacles.iter().all(|r| {
                    let (ox, oy) = (f64::from(r.x), f64::from(r.y));
                    *x + width + gap <= ox
                        || *x >= ox + f64::from(r.width) + gap
                        || y + height + gap <= oy
                        || y >= oy + f64::from(r.height) + gap
                });
                if clear {
                    let cost = (*x - preferred_x).abs() + (y - preferred_y).abs();
                    if best.as_ref().is_none_or(|(old, _)| cost < *old) {
                        best = Some((cost, (*x as f32, y as f32)));
                    }
                }
            }
        }
    }
    best.map(|(_, position)| position)
}

/// 每个识别行的原文范围，不含换行符。
pub(crate) fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut offset = 0;
    text.split('\n')
        .map(|line| {
            let range = offset..offset + line.len();
            offset = range.end + 1;
            range
        })
        .collect()
}

/// 依复制排版合并过的换行（原文 byte 位置）把相邻行分为段落，返回行序号范围。
pub(crate) fn paragraph_groups(lines: &[Range<usize>], joined: &[usize]) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    for (index, line) in lines.iter().enumerate() {
        let continues = index + 1 < lines.len() && joined.binary_search(&line.end).is_ok();
        if !continues {
            groups.push(start..index + 1);
            start = index + 1;
        }
    }
    groups
}

/// 行序号范围对应的原文范围。
pub(crate) fn byte_span(lines: &[Range<usize>], rows: Range<usize>) -> Option<Range<usize>> {
    let first = lines.get(rows.start)?;
    let last = lines.get(rows.end.checked_sub(1)?)?;
    (rows.start < rows.end).then(|| first.start..last.end)
}

/// 与选择相交的行；空行落在选择内部也算。
pub(crate) fn row_selected(line: &Range<usize>, selection: &Range<usize>) -> bool {
    !selection.is_empty()
        && if line.is_empty() {
            selection.start <= line.start && line.start < selection.end
        } else {
            line.start < selection.end && selection.start < line.end
        }
}

/// 包含原文位置的行序号；位置恰在换行处时归前一行。
pub(crate) fn line_of(lines: &[Range<usize>], byte: usize) -> Option<usize> {
    lines.iter().position(|line| byte <= line.end)
}

/// 凸四边形命中，两种顶点绕向均可。
pub(crate) fn quad_contains(quad: &[Point; 4], p: Point) -> bool {
    let mut sign = 0.;
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        let cross = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        if !cross.is_finite() {
            return false;
        }
        if cross != 0. {
            if sign != 0. && cross.signum() != sign {
                return false;
            }
            sign = cross.signum();
        }
    }
    sign != 0.
}

/// 指针下的识别行；行带重叠时取后一行，与原位选字的命中顺序一致。
pub(crate) fn line_at(line_quads: &[Vec<[Point; 4]>], p: Point) -> Option<usize> {
    line_quads
        .iter()
        .rposition(|quads| quads.iter().any(|quad| quad_contains(quad, p)))
}

fn valid(rect: PanelRect) -> bool {
    [rect.x, rect.y, rect.width, rect.height]
        .iter()
        .all(|value| value.is_finite())
        && rect.width > 0.
        && rect.height > 0.
}

/// 把两组连续分段（如原文段落与译文段落）按累计长度比例对齐成若干组：
/// 每组是一段连续的左侧下标与一段连续的右侧下标。段数相同时一一对应；
/// 否则在段数较多的一侧选与较少一侧最接近的段界（总偏差最小），其余段并入相邻组。
pub(crate) fn align_segments(left: &[usize], right: &[usize]) -> Vec<(Range<usize>, Range<usize>)> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    if left.len() == right.len() {
        return (0..left.len()).map(|i| (i..i + 1, i..i + 1)).collect();
    }
    // 内部段界的累计比例：第 i 个值是前 i+1 段的长度占比。
    let bounds = |lengths: &[usize]| {
        let weights: Vec<f64> = lengths.iter().map(|&l| l.max(1) as f64).collect();
        let total: f64 = weights.iter().sum();
        let mut sum = 0.;
        weights[..weights.len() - 1]
            .iter()
            .map(|w| {
                sum += w;
                sum / total
            })
            .collect::<Vec<f64>>()
    };
    let (a, b) = (bounds(left), bounds(right));
    let swapped = a.len() > b.len();
    let (few, many) = if swapped { (&b, &a) } else { (&a, &b) };
    // cost[i][j]：前 i 个少侧段界匹配到多侧前 j 个段界中的最小总偏差。
    let (n, m) = (few.len(), many.len());
    let mut cost = vec![vec![f64::INFINITY; m + 1]; n + 1];
    cost[0].fill(0.);
    for i in 1..=n {
        for j in i..=m {
            let take = cost[i - 1][j - 1] + (few[i - 1] - many[j - 1]).abs();
            cost[i][j] = cost[i][j - 1].min(take);
        }
    }
    let mut picked = vec![0; n];
    let (mut i, mut j) = (n, m);
    while i > 0 {
        if cost[i][j] == cost[i][j - 1] && j > i {
            j -= 1;
        } else {
            picked[i - 1] = j;
            i -= 1;
            j -= 1;
        }
    }
    // 段界 k 位于第 k 段之后，即分组的结束下标为 k+1。
    let mut groups = Vec::with_capacity(n + 1);
    let (mut few_start, mut many_start) = (0, 0);
    let (few_len, many_len) = if swapped {
        (right.len(), left.len())
    } else {
        (left.len(), right.len())
    };
    for (k, &j) in picked.iter().enumerate() {
        groups.push((few_start..k + 1, many_start..j));
        few_start = k + 1;
        many_start = j;
    }
    groups.push((few_start..few_len, many_start..many_len));
    if swapped {
        groups.into_iter().map(|(r, l)| (l, r)).collect()
    } else {
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> PanelRect {
        PanelRect {
            x,
            y,
            width,
            height,
        }
    }
    const MONITOR: PanelRect = PanelRect {
        x: 0.,
        y: 0.,
        width: 1280.,
        height: 720.,
    };

    #[test]
    fn mark_bar_avoids_visible_main_toolbar_and_inside_result_panel() {
        let selected = rect(100., 72., 120., 20.);
        let main = rect(20., 12., 820., 48.);
        // 旧上方(100,26)压一级栏；近处下方比水平跳到远端更合理。
        let position = mark_bar_origin_avoiding(MONITOR, selected, 286., 38., &[main]);
        assert_eq!(position, Some((100., 100.)));
        let bar = rect(position.unwrap().0, position.unwrap().1, 286., 38.);
        assert!(!overlaps(bar, main) && !overlaps(bar, selected));

        let selected = rect(1000., 140., 100., 20.);
        let inside_panel = rect(932., 72., 340., 500.);
        let (x, y) = mark_bar_origin_avoiding(MONITOR, selected, 286., 38., &[inside_panel])
            .expect("一行栏仍有完整位置，不必被后绘的结果面板盖住");
        let bar = rect(x, y, 286., 38.);
        assert!(!overlaps(bar, inside_panel) && !overlaps(bar, selected));
        assert!(bar.x >= 8. && bar.right() <= 1272.);
        assert!(bar.y >= 8. && bar.bottom() <= 712.);
        assert!(bar.bottom() + 8. <= inside_panel.y || bar.right() + 8. <= inside_panel.x);
    }

    #[test]
    fn mark_bar_searches_obstacle_edges_and_has_one_unambiguous_no_room_result() {
        let monitor = rect(0., 0., 400., 250.);
        let selected = rect(40., 140., 60., 20.);
        let main = rect(0., 85., 400., 40.);
        let bottom = rect(0., 168., 400., 82.);
        let (x, y) = mark_bar_origin_avoiding(monitor, selected, 286., 38., &[main, bottom])
            .expect("障碍上沿仍有空间");
        assert_eq!((x, y), (40., 39.));
        let bar = rect(x, y, 286., 38.);
        assert!(bar.bottom() + 8. <= main.y);
        assert!(!overlaps(bar, selected));

        let full_top = rect(0., 0., 400., 140.);
        let full_bottom = rect(0., 160., 400., 90.);
        assert!(
            mark_bar_origin_avoiding(monitor, selected, 286., 38., &[full_top, full_bottom])
                .is_none()
        );
    }

    #[test]
    fn mark_bar_obstacles_keep_negative_monitor_dip_and_reject_invalid_geometry() {
        let monitor = rect(-1280., -100., 1280., 720.);
        let selected = rect(-1100., -28., 120., 20.);
        let main = rect(-1200., -88., 820., 48.);
        assert_eq!(
            mark_bar_origin_avoiding(monitor, selected, 286., 38., &[main]),
            Some((-1100., 0.))
        );
        for invalid in [
            rect(f32::NAN, 0., 20., 20.),
            rect(0., 0., 0., 20.),
            rect(0., 0., 20., -1.),
            rect(f32::MAX, 0., f32::MAX, 20.),
        ] {
            assert!(mark_bar_origin_avoiding(monitor, selected, 286., 38., &[invalid]).is_none());
        }
    }

    #[test]
    fn panel_docks_right_and_spans_selection_through_toolbar() {
        let layout = panel_layout(
            MONITOR,
            rect(120., 84., 700., 470.),
            Some(rect(300., 566., 520., 46.)),
        );
        assert_eq!(layout.side, PanelSide::Right);
        assert_eq!(layout.panel, rect(832., 84., 340., 528.));
    }

    #[test]
    fn panel_flips_left_then_hugs_the_monitor_edge() {
        let left = panel_layout(MONITOR, rect(700., 100., 500., 400.), None);
        assert_eq!(left.side, PanelSide::Left);
        assert_eq!(left.panel.x, 700. - 12. - 340.);
        let inside = panel_layout(MONITOR, rect(20., 20., 1240., 680.), None);
        assert_eq!(inside.side, PanelSide::Inside);
        assert_eq!(inside.panel.right(), 1280. - 8.);
        assert!(inside.panel.bottom() <= 720. - 8.);
    }

    #[test]
    fn short_selection_grows_down_and_stays_on_a_negative_monitor() {
        let monitor = rect(-1920., -200., 1920., 1080.);
        let layout = panel_layout(monitor, rect(-1000., 700., 300., 60.), None);
        assert_eq!(layout.panel.height, 360.);
        assert_eq!(layout.panel.bottom(), monitor.bottom() - 8.);
        assert_eq!(layout.panel.x, -1000. + 300. + 12.);
    }

    #[test]
    fn wide_toolbar_under_the_panel_shortens_it_above_the_toolbar() {
        // 工具栏比选区宽、伸到面板下方时，面板止于工具栏上沿之上，不被遮挡。
        let toolbar = rect(15., 495., 820., 58.);
        let layout = panel_layout(MONITOR, rect(30., 42., 383., 438.), Some(toolbar));
        assert_eq!(layout.side, PanelSide::Right);
        assert_eq!(layout.panel, rect(425., 42., 340., 495. - 8. - 42.));
        assert!(!overlaps(layout.panel, toolbar));
    }

    #[test]
    fn short_selection_moves_panel_beyond_a_wide_toolbar() {
        let toolbar = rect(50., 312., 700., 46.);
        let layout = panel_layout(MONITOR, rect(100., 100., 300., 200.), Some(toolbar));
        assert_eq!(layout.side, PanelSide::Right);
        assert_eq!(layout.panel, rect(762., 100., 340., 360.));
        assert!(!overlaps(layout.panel, toolbar));
    }

    #[test]
    fn full_width_toolbar_pushes_the_inside_panel_off_the_toolbar() {
        let toolbar = rect(8., 600., 1264., 46.);
        let layout = panel_layout(MONITOR, rect(0., 0., 1280., 590.), Some(toolbar));
        assert_eq!(layout.side, PanelSide::Inside);
        assert!(!overlaps(layout.panel, toolbar));
        assert_eq!(layout.panel.bottom(), 600. - 8.);
    }

    #[test]
    fn toolbar_above_the_selection_does_not_stretch_the_panel() {
        let layout = panel_layout(
            MONITOR,
            rect(100., 200., 400., 400.),
            Some(rect(100., 140., 300., 46.)),
        );
        assert_eq!(layout.panel.y, 200.);
        assert_eq!(layout.panel.height, 400.);
    }

    #[test]
    fn tiny_monitor_keeps_panel_finite_and_inside() {
        let monitor = rect(0., 0., 200., 150.);
        let layout = panel_layout(monitor, rect(10., 10., 180., 130.), None);
        assert_eq!(layout.panel.width, 184.);
        assert_eq!(layout.panel.height, 134.);
        assert!(layout.panel.x >= 0. && layout.panel.right() <= 200.);
        assert!(layout.sheet().height >= 1.);
    }

    #[test]
    fn mark_bar_prefers_above_and_falls_below_at_the_top_edge() {
        assert_eq!(
            mark_bar_origin(MONITOR, rect(300., 200., 100., 20.), 280., 36.),
            Some((300., 156.))
        );
        assert_eq!(
            mark_bar_origin(MONITOR, rect(1200., 10., 60., 20.), 280., 36.),
            Some((1280. - 8. - 280., 38.))
        );
    }

    #[test]
    fn mark_bar_rejects_full_height_and_narrow_monitor_for_panel_fallback() {
        // 实际 Ctrl+A／跨行 AABB 无上下空位，不得把浮栏硬夹回文字内。
        assert_eq!(
            mark_bar_origin(MONITOR, rect(20., 8., 100., 704.), 286., 38.),
            None
        );
        assert_eq!(
            mark_bar_origin(
                rect(0., 0., 200., 200.),
                rect(20., 20., 40., 20.),
                286.,
                38.
            ),
            None
        );
        assert_eq!(
            mark_bar_origin(
                rect(0., 0., 1280., 60.),
                rect(20., 8., 100., 44.),
                286.,
                38.
            ),
            None
        );
        // 合格宽度恰好容纳仍允许浮放，不扩大为一律隐藏。
        assert_eq!(
            mark_bar_origin(
                rect(0., 0., 302., 200.),
                rect(20., 80., 40., 20.),
                286.,
                38.
            ),
            Some((8., 34.))
        );
    }

    #[test]
    fn mark_bar_rejects_invalid_geometry_and_keeps_negative_monitor_origin() {
        assert_eq!(
            mark_bar_origin(
                rect(-1280., -720., 1280., 720.),
                rect(-980., -520., 100., 20.),
                286.,
                38.
            ),
            Some((-980., -566.))
        );
        for value in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(mark_bar_origin(MONITOR, rect(20., 80., 40., 20.), value, 38.).is_none());
            assert!(mark_bar_origin(MONITOR, rect(20., 80., 40., 20.), 286., value).is_none());
            assert!(mark_bar_origin(
                rect(0., 0., value, 720.),
                rect(20., 80., 40., 20.),
                286.,
                38.
            )
            .is_none());
            assert!(mark_bar_origin(MONITOR, rect(20., 80., value, 20.), 286., 38.).is_none());
        }
        assert!(mark_bar_origin(MONITOR, rect(f32::NAN, 80., 40., 20.), 286., 38.).is_none());
        assert!(mark_bar_origin(
            rect(f32::MAX, 0., f32::MAX, 720.),
            rect(20., 80., 40., 20.),
            286.,
            38.
        )
        .is_none());
    }

    #[test]
    fn lines_and_paragraphs_follow_joined_newlines() {
        let text = "第一行\n续行\n\n标题\n尾";
        let lines = line_ranges(text);
        assert_eq!(lines.len(), 5);
        assert_eq!(&text[lines[1].clone()], "续行");
        assert!(lines[2].is_empty());
        let groups = paragraph_groups(&lines, &[lines[0].end]);
        assert_eq!(groups, vec![0..2, 2..3, 3..4, 4..5]);
        assert_eq!(byte_span(&lines, 0..2), Some(0..lines[1].end));
        assert_eq!(byte_span(&lines, 3..3), None);
        assert_eq!(line_of(&lines, lines[3].start), Some(3));
        assert_eq!(line_of(&lines, lines[0].end), Some(0));
    }

    #[test]
    fn rows_intersect_selection_including_inner_empty_lines() {
        let lines = line_ranges("ab\n\ncd");
        let selection = 1..6;
        let marked: Vec<_> = lines.iter().map(|l| row_selected(l, &selection)).collect();
        assert_eq!(marked, vec![true, true, true]);
        assert!(!row_selected(&lines[0], &(2..3)));
        assert!(!row_selected(&lines[1], &(3..3)));
    }

    #[test]
    fn hit_test_accepts_both_windings_and_prefers_the_later_line() {
        let p = |x: f32, y: f32| Point { x, y };
        let cw = [p(0., 0.), p(10., 0.), p(10., 5.), p(0., 5.)];
        let ccw = [p(0., 0.), p(0., 5.), p(10., 5.), p(10., 0.)];
        assert!(quad_contains(&cw, p(5., 2.)));
        assert!(quad_contains(&ccw, p(5., 2.)));
        assert!(!quad_contains(&cw, p(11., 2.)));
        let slanted = [p(0., 0.), p(10., 2.), p(9., 7.), p(-1., 5.)];
        assert!(quad_contains(&slanted, p(4., 3.)));
        assert!(!quad_contains(
            &[p(f32::NAN, 0.), p(1., 0.), p(1., 1.), p(0., 1.)],
            p(0.5, 0.5)
        ));
        let lines = vec![
            vec![cw],
            vec![],
            vec![[p(4., 1.), p(20., 1.), p(20., 9.), p(4., 9.)]],
        ];
        assert_eq!(line_at(&lines, p(5., 2.)), Some(2));
        assert_eq!(line_at(&lines, p(1., 1.)), Some(0));
        assert_eq!(line_at(&lines, p(30., 1.)), None);
    }

    #[test]
    fn align_segments_matches_by_length() {
        assert_eq!(
            align_segments(&[5, 5], &[9, 9]),
            vec![(0..1, 0..1), (1..2, 1..2)]
        );
        // 译文把后两段合并成一段。
        assert_eq!(
            align_segments(&[10, 10, 10], &[20, 40]),
            vec![(0..1, 0..1), (1..3, 1..2)]
        );
        // 译文把第一段拆成两段。
        assert_eq!(
            align_segments(&[20, 10], &[10, 10, 10]),
            vec![(0..1, 0..2), (1..2, 2..3)]
        );
        assert_eq!(align_segments(&[3, 4], &[7]), vec![(0..2, 0..1)]);
        assert!(align_segments(&[], &[1]).is_empty());
    }
}
