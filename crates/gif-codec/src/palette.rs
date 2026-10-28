//! 整段 GIF 共用的有界 RGB555 直方图、加权调色板与 O(1) 像素映射。
use crate::Result;

const BINS: usize = 1 << 15;
const COLORS: usize = 256;
const LLOYD_ITERATIONS: usize = 6;

pub(crate) fn key(rgb: [u8; 3]) -> usize {
    (usize::from(rgb[0] >> 3) << 10) | (usize::from(rgb[1] >> 3) << 5) | usize::from(rgb[2] >> 3)
}

/// 位复制覆盖 0..255，避免简单乘 8 或加 4 让白色变灰或黑色变亮。
fn expand5(value: usize) -> u8 {
    ((value << 3) | (value >> 2)) as u8
}

fn key_color(key: usize) -> [u8; 3] {
    [
        expand5(key >> 10),
        expand5((key >> 5) & 31),
        expand5(key & 31),
    ]
}

#[derive(Clone, Copy, Default)]
struct Bin {
    count: u64,
    sum: [u64; 3],
}
impl Bin {
    fn color(self) -> [u8; 3] {
        self.sum
            .map(|sum| ((sum + self.count / 2) / self.count.max(1)) as u8)
    }
}

pub struct Histogram {
    bins: Vec<Bin>,
}
impl Histogram {
    pub fn new() -> Self {
        Self {
            bins: vec![Bin::default(); BINS],
        }
    }

    pub fn sample_rgb(&mut self, rgb: [u8; 3]) {
        let bin = &mut self.bins[key(rgb)];
        bin.count += 1;
        for (sum, channel) in bin.sum.iter_mut().zip(rgb) {
            *sum += u64::from(channel);
        }
    }

    #[cfg(test)]
    pub(crate) fn samples(&self) -> Vec<(u64, [u64; 3])> {
        self.bins.iter().map(|bin| (bin.count, bin.sum)).collect()
    }

    #[cfg(test)]
    fn sample(&mut self, rgba: &[u8], width: usize, x: usize, y: usize) {
        let i = (y * width + x) * 4;
        self.sample_rgb([rgba[i], rgba[i + 1], rgba[i + 2]]);
    }

    /// 每个选中帧 x/y 步长 4；再采边缘及角点，避免非 4 倍数边条完全遗漏。
    #[cfg(test)]
    fn accumulate(&mut self, rgba: &[u8], width: u32, height: u32) {
        let (width, height) = (width as usize, height as usize);
        for y in (0..height).step_by(4) {
            for x in (0..width).step_by(4) {
                self.sample(rgba, width, x, y);
            }
            self.sample(rgba, width, width - 1, y);
        }
        for x in (0..width).step_by(4) {
            self.sample(rgba, width, x, height - 1);
        }
        self.sample(rgba, width, width - 1, height - 1);
    }

    /// progress 可取消；最多 32768 个桶，最多 6 次加权 Lloyd，不缓存视频帧。
    pub fn build(self, progress: &mut dyn FnMut(f32) -> bool) -> Result<Palette> {
        self.build_with_lookup(progress, NearestTree::new)
    }

    /// 限制实际映射使用的颜色数；保留256项表布局，与既有索引压缩流程兼容。
    pub fn build_with_colors(
        self,
        max_colors: usize,
        progress: &mut dyn FnMut(f32) -> bool,
    ) -> Result<Palette> {
        if !(1..=COLORS).contains(&max_colors) {
            return Err("GIF 调色颜色数必须在 1 到 256 之间".into());
        }
        self.build_with_lookup_and_colors(progress, NearestTree::new, max_colors)
    }

    // 只替换最近色查询；分箱、中心更新、终止和取消点均保留原合同。
    // 测试用线性 oracle 走同一流程，核对最终色盘与全部 LUT 字节。
    fn build_with_lookup<L: ColorLookup>(
        self,
        progress: &mut dyn FnMut(f32) -> bool,
        make_lookup: impl Fn(&[[u8; 3]]) -> L,
    ) -> Result<Palette> {
        self.build_with_lookup_and_colors(progress, make_lookup, COLORS)
    }

    fn build_with_lookup_and_colors<L: ColorLookup>(
        self,
        progress: &mut dyn FnMut(f32) -> bool,
        make_lookup: impl Fn(&[[u8; 3]]) -> L,
        max_colors: usize,
    ) -> Result<Palette> {
        let mut entries: Vec<Entry> = self
            .bins
            .iter()
            .enumerate()
            .filter(|(_, bin)| bin.count != 0)
            .map(|(key, bin)| Entry {
                key,
                count: bin.count,
                rgb: bin.color(),
            })
            .collect();
        if entries.is_empty() {
            return Err("录屏没有可生成调色板的画面".into());
        }
        let mut boxes = vec![ColorBox::new(&entries, 0, entries.len())];
        while boxes.len() < max_colors {
            let best = boxes
                .iter()
                .enumerate()
                .filter(|(_, color_box)| color_box.end - color_box.begin >= 2)
                .max_by(|(_, a), (_, b)| a.score().total_cmp(&b.score()))
                .map(|(index, _)| index);
            let Some(best) = best else { break };
            let selected = boxes[best];
            let axis = selected.axis();
            entries[selected.begin..selected.end]
                .sort_unstable_by_key(|entry| (entry.rgb[axis], entry.key));
            let half = selected.weight.div_ceil(2);
            let mut weight = 0_u64;
            let mut split = selected.begin + 1;
            for (offset, entry) in entries[selected.begin..selected.end - 1].iter().enumerate() {
                weight += entry.count;
                split = selected.begin + offset + 1;
                if weight >= half {
                    break;
                }
            }
            boxes[best] = ColorBox::new(&entries, selected.begin, split);
            boxes.push(ColorBox::new(&entries, split, selected.end));
            if !progress(0.2 * boxes.len() as f32 / max_colors as f32) {
                return Err("已取消 GIF 转换".into());
            }
        }
        let mut colors: Vec<[u8; 3]> = boxes
            .iter()
            .map(|color_box| color_box.average(&entries))
            .collect();
        for iteration in 0..LLOYD_ITERATIONS {
            let nearest_colors = make_lookup(&colors);
            let mut sums = vec![[0_u64; 3]; colors.len()];
            let mut weights = vec![0_u64; colors.len()];
            for (index, entry) in entries.iter().enumerate() {
                let nearest = nearest_colors.nearest(entry.rgb);
                weights[nearest] += entry.count;
                for (sum, channel) in sums[nearest].iter_mut().zip(entry.rgb) {
                    *sum += u64::from(channel) * entry.count;
                }
                if index.is_multiple_of(256)
                    && !progress(
                        0.2 + 0.55 * (iteration as f32 + index as f32 / entries.len() as f32)
                            / LLOYD_ITERATIONS as f32,
                    )
                {
                    return Err("已取消 GIF 转换".into());
                }
            }
            let mut changed = false;
            for (index, color) in colors.iter_mut().enumerate() {
                if weights[index] != 0 {
                    let updated =
                        sums[index].map(|sum| ((sum + weights[index] / 2) / weights[index]) as u8);
                    changed |= updated != *color;
                    *color = updated;
                }
            }
            if !changed {
                break;
            }
        }
        let nearest_colors = make_lookup(&colors);
        let mut lookup = vec![0_u8; BINS];
        for (index, destination) in lookup.iter_mut().enumerate() {
            // 出现过的桶用实采均值，纯色准确保留；未出现的桶用覆盖全域的位扩展中心。
            let rgb = if self.bins[index].count == 0 {
                key_color(index)
            } else {
                self.bins[index].color()
            };
            *destination = nearest_colors.nearest(rgb) as u8;
            if index.is_multiple_of(256) && !progress(0.75 + 0.25 * index as f32 / BINS as f32) {
                return Err("已取消 GIF 转换".into());
            }
        }
        let mut table = Vec::with_capacity(COLORS * 3);
        for color in &colors {
            table.extend_from_slice(color);
        }
        table.resize(COLORS * 3, 0);
        if !progress(1.) {
            return Err("已取消 GIF 转换".into());
        }
        Ok(Palette { table, lookup })
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
struct Entry {
    key: usize,
    count: u64,
    rgb: [u8; 3],
}

#[derive(Clone, Copy)]
struct ColorBox {
    begin: usize,
    end: usize,
    weight: u64,
    error: [f64; 3],
}
impl ColorBox {
    fn new(entries: &[Entry], begin: usize, end: usize) -> Self {
        let mut weight = 0_u64;
        let mut sum = [0_u64; 3];
        let mut square = [0_u64; 3];
        for entry in &entries[begin..end] {
            weight += entry.count;
            for channel in 0..3 {
                let value = u64::from(entry.rgb[channel]);
                sum[channel] += value * entry.count;
                square[channel] += value * value * entry.count;
            }
        }
        let error = std::array::from_fn(|channel| {
            (square[channel] as f64 - (sum[channel] as f64).powi(2) / weight.max(1) as f64).max(0.)
        });
        Self {
            begin,
            end,
            weight,
            error,
        }
    }
    fn axis(self) -> usize {
        (0..3)
            .max_by(|a, b| self.error[*a].total_cmp(&self.error[*b]))
            .unwrap()
    }
    fn score(self) -> f64 {
        self.error[self.axis()]
    }
    fn average(self, entries: &[Entry]) -> [u8; 3] {
        let mut sums = [0_u64; 3];
        for entry in &entries[self.begin..self.end] {
            for (sum, channel) in sums.iter_mut().zip(entry.rgb) {
                *sum += u64::from(channel) * entry.count;
            }
        }
        sums.map(|sum| ((sum + self.weight / 2) / self.weight.max(1)) as u8)
    }
}

trait ColorLookup {
    fn nearest(&self, rgb: [u8; 3]) -> usize;
}

const NO_NODE: u16 = u16::MAX;

#[derive(Clone, Copy)]
struct Point {
    rgb: [u8; 3],
    index: u8,
}

#[derive(Clone, Copy)]
struct Node {
    point: Point,
    lower: [u8; 3],
    upper: [u8; 3],
    first_index: u8,
    children: [u16; 2],
}

/// 平衡 KD 树的子树带 RGB AABB；最多 256 节点，查询无需分配或线程。
/// 只有严格不可能改善距离/原索引时才剪枝，保持线性查询的等距合同。
struct NearestTree {
    nodes: Vec<Node>,
    root: u16,
}

impl NearestTree {
    fn new(colors: &[[u8; 3]]) -> Self {
        assert!(!colors.is_empty() && colors.len() <= COLORS);
        let mut points: Vec<Point> = colors
            .iter()
            .enumerate()
            .map(|(index, rgb)| Point {
                rgb: *rgb,
                index: index as u8,
            })
            .collect();
        let mut nodes = Vec::with_capacity(points.len());
        let root = Self::build_nodes(&mut points, &mut nodes);
        Self { nodes, root }
    }

    fn build_nodes(points: &mut [Point], nodes: &mut Vec<Node>) -> u16 {
        if points.is_empty() {
            return NO_NODE;
        }
        let mut lower = [u8::MAX; 3];
        let mut upper = [0; 3];
        let mut first_index = u8::MAX;
        for point in points.iter() {
            for channel in 0..3 {
                lower[channel] = lower[channel].min(point.rgb[channel]);
                upper[channel] = upper[channel].max(point.rgb[channel]);
            }
            first_index = first_index.min(point.index);
        }
        let mut axis = 0;
        for channel in 1..3 {
            if upper[channel] - lower[channel] > upper[axis] - lower[axis] {
                axis = channel;
            }
        }
        points.sort_unstable_by_key(|point| (point.rgb[axis], point.index));
        let middle = points.len() / 2;
        let point = points[middle];
        let index = nodes.len() as u16;
        nodes.push(Node {
            point,
            lower,
            upper,
            first_index,
            children: [NO_NODE; 2],
        });
        let (left_and_middle, right) = points.split_at_mut(middle + 1);
        let left = Self::build_nodes(&mut left_and_middle[..middle], nodes);
        let right = Self::build_nodes(right, nodes);
        nodes[usize::from(index)].children = [left, right];
        index
    }

    fn child_bound(&self, child: u16, rgb: [u8; 3]) -> (u32, u8) {
        if child == NO_NODE {
            return (u32::MAX, u8::MAX);
        }
        let node = &self.nodes[usize::from(child)];
        (box_distance(rgb, node.lower, node.upper), node.first_index)
    }

    fn search(&self, index: u16, rgb: [u8; 3], bound: (u32, u8), best: &mut (u32, u8)) {
        if index == NO_NODE || bound >= *best {
            return;
        }
        let node = &self.nodes[usize::from(index)];
        let candidate = (distance(rgb, node.point.rgb), node.point.index);
        if candidate < *best {
            *best = candidate;
        }
        let [left, right] = node.children;
        let left_bound = self.child_bound(left, rgb);
        let right_bound = self.child_bound(right, rgb);
        // 较近的 AABB 先查；相等时原最小索引先查，可尽早安全剪去重复色。
        if left_bound <= right_bound {
            self.search(left, rgb, left_bound, best);
            self.search(right, rgb, right_bound, best);
        } else {
            self.search(right, rgb, right_bound, best);
            self.search(left, rgb, left_bound, best);
        }
    }
}

impl ColorLookup for NearestTree {
    fn nearest(&self, rgb: [u8; 3]) -> usize {
        let mut best = (u32::MAX, u8::MAX);
        self.search(self.root, rgb, self.child_bound(self.root, rgb), &mut best);
        usize::from(best.1)
    }
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    let r = u32::from(a[0].abs_diff(b[0]));
    let g = u32::from(a[1].abs_diff(b[1]));
    let b = u32::from(a[2].abs_diff(b[2]));
    r * r + g * g + b * b
}

fn box_distance(rgb: [u8; 3], lower: [u8; 3], upper: [u8; 3]) -> u32 {
    distance(
        rgb,
        [
            rgb[0].clamp(lower[0], upper[0]),
            rgb[1].clamp(lower[1], upper[1]),
            rgb[2].clamp(lower[2], upper[2]),
        ],
    )
}

pub struct Palette {
    /// GIF 全局 RGB 色盘，始终补齐为 256×3 bytes。
    pub table: Vec<u8>,
    lookup: Vec<u8>,
}
impl Palette {
    pub fn index(&self, rgb: [u8; 3]) -> u8 {
        self.lookup[key(rgb)]
    }

    /// 使用第一遍缓存的 RGB555 桶，映射仍为原全局调色板查找表。
    /// keys 由 Scaler::accumulate_keys 生成，每项范围为 0..32768。
    pub fn index_keys(&self, keys: &[u16]) -> Vec<u8> {
        keys.iter()
            .map(|key| self.lookup[usize::from(*key)])
            .collect()
    }

    #[cfg(test)]
    fn map(&self, rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4)
            .map(|pixel| self.index([pixel[0], pixel[1], pixel[2]]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 保留原线性扫描作为独立 oracle，不调用树的距离/剪枝辅助函数。
    fn nearest_linear(colors: &[[u8; 3]], rgb: [u8; 3]) -> usize {
        let mut best = (0, u32::MAX);
        for (index, color) in colors.iter().enumerate() {
            let distance: u32 = rgb
                .into_iter()
                .zip(*color)
                .map(|(a, b)| u32::from(a.abs_diff(b)).pow(2))
                .sum();
            if distance < best.1 {
                best = (index, distance);
            }
        }
        best.0
    }

    struct Linear(Vec<[u8; 3]>);
    impl ColorLookup for Linear {
        fn nearest(&self, rgb: [u8; 3]) -> usize {
            nearest_linear(&self.0, rgb)
        }
    }

    fn fixture_colors(count: usize) -> Vec<[u8; 3]> {
        let mut state = 0x7415_3c87_u32;
        (0..count)
            .map(|index| {
                let rgb = std::array::from_fn(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                });
                if index.is_multiple_of(17) {
                    [0, 128, 255]
                } else {
                    rgb
                }
            })
            .collect()
    }

    fn fixture_histogram() -> Histogram {
        let mut histogram = Histogram::new();
        for (index, rgb) in fixture_colors(1536).into_iter().enumerate() {
            for _ in 0..1 + index % 11 {
                histogram.sample_rgb(rgb);
            }
        }
        for rgb in [[0, 0, 0], [255, 255, 255], [239, 64, 207]] {
            histogram.sample_rgb(rgb);
        }
        histogram
    }

    #[test]
    fn gif_exact_nearest_matches_linear_for_entire_rgb555_domain() {
        for count in [1, 2, 17, COLORS] {
            let colors = fixture_colors(count);
            let tree = NearestTree::new(&colors);
            for key in 0..BINS {
                let rgb = key_color(key);
                assert_eq!(
                    tree.nearest(rgb),
                    nearest_linear(&colors, rgb),
                    "色数 {count}，RGB555 {key}"
                );
            }
        }
    }

    #[test]
    fn gif_exact_nearest_retains_first_index_for_duplicates_and_ties() {
        let duplicates = vec![[10, 20, 30]; COLORS];
        let tree = NearestTree::new(&duplicates);
        for rgb in [[0, 0, 0], [10, 20, 30], [255, 255, 255], [7, 91, 240]] {
            assert_eq!(tree.nearest(rgb), 0);
        }
        for colors in [
            vec![[0, 0, 0], [2, 0, 0]],
            vec![[2, 0, 0], [0, 0, 0]],
            vec![[0, 2, 0], [2, 0, 0], [0, 0, 2], [0, 2, 0]],
            vec![[0, 0, 0], [2, 2, 2], [0, 2, 0], [2, 0, 2]],
        ] {
            let tree = NearestTree::new(&colors);
            for rgb in [[1, 0, 0], [1, 1, 1], [0, 1, 1], [2, 1, 1]] {
                assert_eq!(tree.nearest(rgb), nearest_linear(&colors, rgb));
            }
        }
    }

    #[test]
    fn gif_palette_and_full_lookup_match_linear_oracle_byte_for_byte() {
        let mut exact_progress = Vec::new();
        let exact = fixture_histogram()
            .build(&mut |value| {
                exact_progress.push(value);
                true
            })
            .unwrap();
        let mut linear_progress = Vec::new();
        let linear = fixture_histogram()
            .build_with_lookup(
                &mut |value| {
                    linear_progress.push(value);
                    true
                },
                |colors| Linear(colors.to_vec()),
            )
            .unwrap();
        assert_eq!(exact.table, linear.table);
        assert_eq!(exact.lookup, linear.lookup);
        assert_eq!(exact_progress, linear_progress);
        assert_eq!(exact.table.len(), COLORS * 3);
        assert_eq!(exact.lookup.len(), BINS);
    }

    #[test]
    fn gif_palette_color_limit_changes_indices_and_preserves_default_output() {
        let legacy = fixture_histogram().build(&mut |_| true).unwrap();
        let explicit = fixture_histogram()
            .build_with_colors(COLORS, &mut |_| true)
            .unwrap();
        assert_eq!(legacy.table, explicit.table);
        assert_eq!(legacy.lookup, explicit.lookup);
        let mut previous = None;
        for count in [1, 64, 128, COLORS] {
            let palette = fixture_histogram()
                .build_with_colors(count, &mut |_| true)
                .unwrap();
            assert_eq!(palette.table.len(), COLORS * 3);
            assert_eq!(palette.lookup.len(), BINS);
            assert!(palette
                .lookup
                .iter()
                .all(|index| usize::from(*index) < count));
            let used: std::collections::BTreeSet<_> = palette.lookup.iter().copied().collect();
            assert!(used.len() <= count);
            if count == 1 {
                assert_eq!(used.len(), 1);
            } else {
                assert!(used.len() > 1);
            }
            if let Some(previous) = previous {
                assert_ne!(palette.lookup, previous);
            }
            previous = Some(palette.lookup);
        }
    }

    #[test]
    fn gif_palette_color_limit_rejects_invalid_counts_and_keeps_cancellation() {
        for count in [0, COLORS + 1, usize::MAX] {
            assert!(fixture_histogram()
                .build_with_colors(count, &mut |_| panic!("无效颜色数不应进入生成"))
                .is_err());
        }
        for count in [64, 128] {
            for stop_at in [0.05, 0.40, 0.80, 1.0] {
                let result =
                    fixture_histogram().build_with_colors(count, &mut |value| value < stop_at);
                assert_eq!(result.err().unwrap().to_string(), "已取消 GIF 转换");
            }
        }
    }

    #[test]
    fn gif_palette_empty_input_and_each_build_stage_keep_cancellation() {
        assert_eq!(
            Histogram::new()
                .build(&mut |_| true)
                .err()
                .unwrap()
                .to_string(),
            "录屏没有可生成调色板的画面"
        );
        for stop_at in [0.05, 0.40, 0.80, 1.0] {
            let mut cancelled_at = None;
            let result = fixture_histogram().build(&mut |value| {
                if value >= stop_at {
                    cancelled_at = Some(value);
                    false
                } else {
                    true
                }
            });
            assert_eq!(result.err().unwrap().to_string(), "已取消 GIF 转换");
            assert!(cancelled_at.is_some_and(|value| value >= stop_at));
        }
    }

    fn reconstruct(palette: &Palette, rgba: &[u8]) -> Vec<[u8; 3]> {
        palette
            .map(rgba)
            .into_iter()
            .map(|index| {
                let start = usize::from(index) * 3;
                palette.table[start..start + 3].try_into().unwrap()
            })
            .collect()
    }

    #[test]
    fn gif_global_palette_includes_later_color_and_non4_edge() {
        let mut histogram = Histogram::new();
        let first = [0, 0, 0, 255].repeat(5 * 3);
        histogram.accumulate(&first, 5, 3);
        let mut later = first.clone();
        later[(3 * 5 - 1) * 4..].copy_from_slice(&[255, 32, 160, 255]);
        histogram.accumulate(&later, 5, 3);
        let palette = histogram.build(&mut |_| true).unwrap();
        assert_eq!(reconstruct(&palette, &later).last(), Some(&[255, 32, 160]));
        assert_eq!(reconstruct(&palette, &first)[0], [0, 0, 0]);
    }

    #[test]
    fn gif_palette_preserves_black_white_and_measures_gradient_error() {
        assert_eq!(key_color(0), [0, 0, 0]);
        assert_eq!(key_color(BINS - 1), [255, 255, 255]);
        let mut rgba = Vec::new();
        for value in 0..=255_u8 {
            rgba.extend_from_slice(&[value, value, value, 255]);
        }
        let mut histogram = Histogram::new();
        // 多组采样相位确保此模型覆盖256个灰度，桶内均值的量化误差应仅≤4。
        for phase in 0..4 {
            let rotated = rgba[phase * 4..]
                .iter()
                .chain(&rgba[..phase * 4])
                .copied()
                .collect::<Vec<_>>();
            histogram.accumulate(&rotated, 256, 1);
        }
        let palette = histogram.build(&mut |_| true).unwrap();
        let recovered = reconstruct(&palette, &rgba);
        let worst = recovered
            .iter()
            .enumerate()
            .map(|(value, color)| color[0].abs_diff(value as u8))
            .max()
            .unwrap();
        assert!(worst <= 4, "灰度最大误差 {worst}");
        let mut extremes = Histogram::new();
        extremes.accumulate(&[0, 0, 0, 255, 255, 255, 255, 255], 2, 1);
        let extremes = extremes.build(&mut |_| true).unwrap();
        assert_eq!(
            reconstruct(&extremes, &[0, 0, 0, 255, 255, 255, 255, 255]),
            [[0, 0, 0], [255, 255, 255]]
        );
    }

    #[test]
    fn gif_palette_build_is_cancellable() {
        let mut histogram = Histogram::new();
        histogram.accumulate(&[20, 40, 80, 255], 1, 1);
        assert!(histogram.build(&mut |_| false).is_err());
    }
}
