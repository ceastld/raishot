//! 多码二维码识别：局部阈值二值化后按 1:1:3:1:1 找定位角，三个定位角配成直角等腰三角形、
//! 再核对两条时序线，推出每个码的区域，只把这些小区域交给 quirc 解码。
//!
//! 整图直接交给 quirc 有三处硬伤：最多记录 32 个定位角和 8 个码，按从上到下的顺序
//! 先到先得，文字里“回”字一类的形状会先占满名额；阈值是整图 Otsu，深浅背景混排时
//! 只有一部分码能二值化正确；不支持深底浅码。区域解码同时省掉整图连通域标记，
//! 没有码的图只花一次线性扫描。
use super::QrHit;

/// quirc 在模块小于约 3 像素时容易丢定位角，区域内先放大到约 4 像素。
const MIN_MODULE_PX: f32 = 3.;
/// 定位角中心距按模块计：版本 1 为 14，版本 40 为 170；留出透视余量。
const MIN_LEG_MODULES: f32 = 11.;
const MAX_LEG_MODULES: f32 = 185.;
/// 区域在码外再留的模块数（定位角中心到码边 3.5 + 静区 4 + 透视余量）。
const PAD_MODULES: f32 = 9.;
/// 一张图最多解这么多个区域，防止病态图片无限耗时。
const MAX_REGIONS: usize = 256;
/// 一个码都没解出时，再试几个时序线没通过的三角形（强透视、模糊的码）。
const MAX_UNVERIFIED_REGIONS: usize = 8;
/// 局部阈值窗口半径：须宽于定位角中心块（3 模块），原图上覆盖约 20 像素以内的模块。
const WINDOW_RADIUS: usize = 36;
/// 模块更大的码在 1/4 缩略图上找定位角。
const COARSE: usize = 4;
const DARK: u8 = 1;
const LIGHT: u8 = 2;

#[derive(Clone, Copy, Debug)]
struct Finder {
    x: f32,
    y: f32,
    /// 模块边长（像素）。
    module: f32,
    /// 深底浅码。
    inverted: bool,
    hits: u32,
    last_row: usize,
}

pub(super) fn scan(gray: &[u8], width: usize, height: usize) -> Vec<QrHit> {
    if width < 21 || height < 21 || gray.len() != width * height {
        return Vec::new();
    }
    let mut finders = finders_at(gray, width, height, 1.);
    if width.min(height) >= COARSE * 48 {
        let (small, w, h) = downsample(gray, width, height);
        for f in finders_at(&small, w, h, COARSE as f32) {
            if !finders.iter().any(|g| {
                g.inverted == f.inverted && (g.x - f.x).abs() + (g.y - f.y).abs() < f.module * 2.
            }) {
                finders.push(f);
            }
        }
    }
    // 形状误差小、腰短的三角形先选；每个定位角只属于一个码。时序线通过的直接解码，
    // 没通过的只在一个码都没解出时再试几个。
    let mut claimed: Vec<(f32, f32)> = Vec::new();
    let mut verified = Vec::new();
    let mut unverified = Vec::new();
    for triangle in triangles(&finders) {
        if verified.len() >= MAX_REGIONS {
            break;
        }
        if triangle.iter().any(|f| claimed.contains(&(f.x, f.y))) {
            continue;
        }
        let threshold = finder_threshold(gray, width, height, triangle);
        if timing_ok(gray, width, height, triangle, threshold) {
            claimed.extend(triangle.iter().map(|f| (f.x, f.y)));
            verified.push((triangle, threshold));
        } else if unverified.len() < MAX_UNVERIFIED_REGIONS {
            unverified.push((triangle, threshold));
        }
    }
    let hits = decode_all(gray, width, height, &verified);
    if hits.is_empty() {
        return decode_all(gray, width, height, &unverified);
    }
    hits
}

/// quirc 每个码都要做一轮透视微调，耗时和区域大小关系不大；多个区域分到多线程解。
fn decode_all(gray: &[u8], width: usize, height: usize, jobs: &[([Finder; 3], u8)]) -> Vec<QrHit> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
        .min(jobs.len());
    let next = AtomicUsize::new(0);
    let work = || {
        let mut found = Vec::new();
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(&(triangle, threshold)) = jobs.get(index) else {
                return found;
            };
            found.extend(
                decode_region(gray, width, height, triangle, threshold)
                    .into_iter()
                    .map(|hit| (index, hit)),
            );
        }
    };
    let mut found = if threads <= 1 {
        work()
    } else {
        std::thread::scope(|scope| {
            let workers: Vec<_> = (1..threads).map(|_| scope.spawn(work)).collect();
            let mut found = work();
            for worker in workers {
                found.extend(worker.join().unwrap_or_default());
            }
            found
        })
    };
    // 按三角形顺序合并，结果与单线程一致。
    found.sort_by_key(|(index, _)| *index);
    let mut hits = Vec::new();
    for (_, hit) in found {
        push_unique(&mut hits, hit);
    }
    hits
}

/// 一次二值化同时得到浅底深码与深底浅码两张图，两种极性并行找定位角；
/// 坐标乘以 scale 回到原图。
fn finders_at(gray: &[u8], width: usize, height: usize, scale: f32) -> Vec<Finder> {
    let map = binarize(gray, width, height);
    let map = &map;
    let (mut finders, inverted) = std::thread::scope(|scope| {
        let inverted = scope.spawn(move || find_finders(map, width, height, true));
        (
            find_finders(map, width, height, false),
            inverted.join().unwrap_or_default(),
        )
    });
    finders.extend(inverted);
    for f in &mut finders {
        f.x *= scale;
        f.y *= scale;
        f.module *= scale;
    }
    finders
}

/// 按 COARSE×COARSE 块取平均的缩略图。
fn downsample(gray: &[u8], width: usize, height: usize) -> (Vec<u8>, usize, usize) {
    let (w, h) = (width / COARSE, height / COARSE);
    let mut sums = vec![0u32; w];
    let mut out = vec![0u8; w * h];
    for (y, line) in out.chunks_exact_mut(w).enumerate() {
        sums.fill(0);
        for row in gray[y * COARSE * width..].chunks_exact(width).take(COARSE) {
            for (sum, block) in sums.iter_mut().zip(row.chunks_exact(COARSE)) {
                *sum += block.iter().map(|&v| u32::from(v)).sum::<u32>();
            }
        }
        for (out, sum) in line.iter_mut().zip(&sums) {
            *out = (sum / (COARSE * COARSE) as u32) as u8;
        }
    }
    (out, w, h)
}

/// 从直角顶点沿两腰、向第三个定位角偏 3 个模块，是码的两条时序线：深浅逐模块交替。
/// 跨码拼出的三角形、文字凑出的假三角形在这里会碰到静区或成片的字，直接排除。
fn timing_ok(
    gray: &[u8],
    width: usize,
    height: usize,
    [a, b, c]: [Finder; 3],
    threshold: u8,
) -> bool {
    let dark = |x: f32, y: f32| {
        if x < 0. || y < 0. || x >= width as f32 || y >= height as f32 {
            return None;
        }
        Some((gray[y as usize * width + x as usize] < threshold) != a.inverted)
    };
    let leg = |b: &Finder, c: &Finder| {
        let module = (a.module + b.module + c.module) / 3.;
        let (ux, uy) = (b.x - a.x, b.y - a.y);
        let length = (ux * ux + uy * uy).sqrt();
        let (vx, vy) = (c.x - a.x, c.y - a.y);
        let side = (vx * vx + vy * vy).sqrt();
        let (ux, uy, vx, vy) = (ux / length, uy / length, vx / side, vy / side);
        // 时序线从本端定位角外 1 个模块到另一端定位角之前，两端各让半个模块。
        let (from, to) = (4. * module, length - 4. * module);
        let step = (module / 3.).min(1.);
        let (ox, oy) = (a.x + vx * 3. * module, a.y + vy * 3. * module);
        // 逐段累计；任何一段过长（静区、成片的字）立即否决，假三角形多在前几个模块就退出。
        // 两端的段可能被截断，只对中间各段要求不过短。
        let (longest, shortest) = (2.2 * module, 0.3 * module);
        let (mut runs, mut run, mut last) = (0, 0., None);
        let mut t = from;
        while t <= to {
            let Some(color) = dark(ox + ux * t, oy + uy * t) else {
                return false;
            };
            if last == Some(color) {
                run += step;
                if run > longest {
                    return false;
                }
            } else {
                if runs > 0 && run < shortest {
                    return false;
                }
                runs += 1;
                run = step;
                last = Some(color);
            }
            t += step;
        }
        let expected = (to - from) / module;
        runs as f32 >= (expected - 1.).max(3.) * 0.7
    };
    leg(&b, &c) && leg(&c, &b)
}

/// 同一个码偶尔被相邻区域各解一次；内容相同且中心相近只保留一份。
pub(super) fn push_unique(hits: &mut Vec<QrHit>, hit: QrHit) {
    let (cx, cy) = hit.center();
    if !hits.iter().any(|old| {
        let (ox, oy) = old.center();
        old.text == hit.text && (ox - cx).abs() + (oy - cy).abs() < hit.side() / 2.
    }) {
        hits.push(hit);
    }
}

/// 行内滑动窗口阈值（同 quirc 新版思路）：每个像素和左右 WINDOW_RADIUS 内的均值比较，
/// 低于均值 95% 记 DARK，高于 105% 记 LIGHT（深底浅码的“深色”）。只用于找定位角，
/// 解码另按定位角实测颜色取阈值。均匀区两者都不是：浅底深码的白静区、深底浅码的黑静区
/// 都能和定位角外框分开。按行分段多线程，前缀和避免逐像素除法。
fn binarize(gray: &[u8], width: usize, height: usize) -> Vec<u8> {
    let radius = WINDOW_RADIUS;
    let mut dark = vec![0u8; gray.len()];
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
        .min(height / 64)
        .max(1);
    let rows = height.div_ceil(threads);
    std::thread::scope(|scope| {
        for (src, out) in gray.chunks(rows * width).zip(dark.chunks_mut(rows * width)) {
            scope.spawn(move || {
                let mut prefix = vec![0u32; width + 1];
                for (row, out) in src.chunks_exact(width).zip(out.chunks_exact_mut(width)) {
                    for (x, &v) in row.iter().enumerate() {
                        prefix[x + 1] = prefix[x] + u32::from(v);
                    }
                    for (x, (&v, out)) in row.iter().zip(out.iter_mut()).enumerate() {
                        let lo = x.saturating_sub(radius);
                        let hi = (x + radius + 1).min(width);
                        let sum = prefix[hi] - prefix[lo];
                        let scaled = u32::from(v) * (hi - lo) as u32 * 100;
                        *out = if scaled < sum * 95 {
                            DARK
                        } else if scaled > sum * 105 {
                            LIGHT
                        } else {
                            0
                        };
                    }
                }
            });
        }
    });
    dark
}

/// 五段游程是否符合 1:1:3:1:1。返回模块边长。
fn finder_ratio(runs: [u32; 5]) -> Option<f32> {
    let total: u32 = runs.iter().sum();
    if total < 7 {
        return None;
    }
    let module = total as f32 / 7.;
    let variance = (module * 0.6).max(0.75);
    let ok = [0, 1, 3, 4]
        .iter()
        .all(|&i| (runs[i] as f32 - module).abs() <= variance)
        && (runs[2] as f32 - 3. * module).abs() <= 2. * variance;
    ok.then_some(module)
}

/// 从 (x, y) 沿列向上、向下各数两段异色和中心同色段，核对竖向比例。返回竖向中心与模块。
fn cross_check_vertical(
    dark: &[u8],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    bit: u8,
    max_run: u32,
) -> Option<(f32, f32)> {
    let at = |y: usize| u8::from(dark[y * width + x] & bit != 0);
    let stone = 1;
    if at(y) != stone {
        return None;
    }
    let mut up = [0u32; 3];
    let mut row = y as isize;
    for (i, color) in [stone, stone ^ 1, stone].into_iter().enumerate() {
        while row >= 0 && at(row as usize) == color && up[i] <= max_run {
            up[i] += 1;
            row -= 1;
        }
        if up[i] == 0 || up[i] > max_run {
            return None;
        }
    }
    let mut down = [0u32; 3];
    let mut row = y + 1;
    for (i, color) in [stone, stone ^ 1, stone].into_iter().enumerate() {
        // 中心段向下可以为 0（种子正好在中心段最后一行）。
        while row < height && at(row) == color && down[i] <= max_run {
            down[i] += 1;
            row += 1;
        }
        if (i > 0 && down[i] == 0) || down[i] > max_run {
            return None;
        }
    }
    let runs = [up[2], up[1], up[0] + down[0], down[1], down[2]];
    let module = finder_ratio(runs)?;
    let top = y as f32 + 1. - up[0] as f32;
    Some((top + (up[0] + down[0]) as f32 / 2., module))
}

/// 逐行找 1:1:3:1:1 游程，竖向复核后把相邻命中合并为一个定位角。
fn find_finders(dark: &[u8], width: usize, height: usize, inverted: bool) -> Vec<Finder> {
    let bit = if inverted { LIGHT } else { DARK };
    let mut active: Vec<Finder> = Vec::new();
    let mut done: Vec<Finder> = Vec::new();
    let mut starts: Vec<u32> = Vec::with_capacity(width / 2);
    for y in 0..height {
        let row = &dark[y * width..][..width];
        starts.clear();
        starts.push(0);
        for x in 1..width {
            if (row[x] ^ row[x - 1]) & bit != 0 {
                starts.push(x as u32);
            }
        }
        starts.push(width as u32);
        let runs = starts.len() - 1;
        for i in 0..runs.saturating_sub(4) {
            let r = |k: usize| starts[i + k + 1] - starts[i + k];
            let lens = [r(0), r(1), r(2), r(3), r(4)];
            // 中心块必须是本图的深色（反色图里就是浅色）。
            if row[starts[i + 2] as usize] & bit == 0 {
                continue;
            }
            let Some(module) = finder_ratio(lens) else {
                continue;
            };
            let left = starts[i + 2];
            let center_x = left as f32 + lens[2] as f32 / 2.;
            let total: u32 = lens.iter().sum();
            let Some((center_y, v_module)) =
                cross_check_vertical(dark, width, height, center_x as usize, y, bit, total)
            else {
                continue;
            };
            if !(0.5..=2.).contains(&(v_module / module)) {
                continue;
            }
            let module = (module + v_module) / 2.;
            // 同一定位角会在中心段每一行命中一次，按位置合并。
            if let Some(f) = active.iter_mut().find(|f| {
                f.inverted == inverted
                    && (f.x - center_x).abs() <= f.module.max(module) * 1.5
                    && (f.y - center_y).abs() <= f.module.max(module) * 1.5
                    && (0.6..=1.7).contains(&(module / f.module))
            }) {
                let n = f.hits as f32;
                f.x = (f.x * n + center_x) / (n + 1.);
                f.y = (f.y * n + center_y) / (n + 1.);
                f.module = (f.module * n + module) / (n + 1.);
                f.hits += 1;
                f.last_row = y;
            } else {
                active.push(Finder {
                    x: center_x,
                    y: center_y,
                    module,
                    inverted,
                    hits: 1,
                    last_row: y,
                });
            }
        }
        // 已离开当前行足够远的定位角不再参与合并，保持活动列表很短。
        let mut i = 0;
        while i < active.len() {
            if (y - active[i].last_row) as f32 > active[i].module * 4. + 2. {
                done.push(active.swap_remove(i));
            } else {
                i += 1;
            }
        }
    }
    done.extend(active);
    done
}

/// 枚举能构成二维码三个定位角的组合：同极性、模块相近、两腰近似相等且夹角近似直角。
/// 返回 [直角顶点, 腰端点, 腰端点]，按形状误差、再按腰长从小到大排序。
fn triangles(finders: &[Finder]) -> Vec<[Finder; 3]> {
    let similar = |a: &Finder, b: &Finder| {
        a.inverted == b.inverted && (0.66..=1.5).contains(&(a.module / b.module))
    };
    let mut found: Vec<(f32, f32, [Finder; 3])> = Vec::new();
    let mut legs: Vec<(usize, f32, f32, f32)> = Vec::new();
    for (ia, a) in finders.iter().enumerate() {
        legs.clear();
        for (ib, b) in finders.iter().enumerate() {
            if ia == ib || !similar(a, b) {
                continue;
            }
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let length = (dx * dx + dy * dy).sqrt();
            let modules = length / ((a.module + b.module) / 2.);
            if (MIN_LEG_MODULES..=MAX_LEG_MODULES).contains(&modules) {
                legs.push((ib, dx, dy, length));
            }
        }
        for (k, &(ib, bx, by, bl)) in legs.iter().enumerate() {
            for &(ic, cx, cy, cl) in &legs[k + 1..] {
                let ratio = bl / cl;
                if !(0.8..=1.25).contains(&ratio) || !similar(&finders[ib], &finders[ic]) {
                    continue;
                }
                let cos = (bx * cx + by * cy) / (bl * cl);
                if cos.abs() > 0.2 {
                    continue;
                }
                let error = (1. - ratio).abs() + cos.abs();
                found.push((error, bl.max(cl), [*a, finders[ib], finders[ic]]));
            }
        }
    }
    // 误差按 0.05 分档：形状同样好的组合里先解小三角形，跨码拼出的大三角形排在后面。
    found.sort_by(|l, r| {
        let bucket = |e: f32| (e / 0.05) as u32;
        bucket(l.0).cmp(&bucket(r.0)).then(l.1.total_cmp(&r.1))
    });
    found.into_iter().map(|(_, _, t)| t).collect()
}

/// 按三个定位角推出码的外接框，裁出（必要时放大、反色）后交给 quirc。
fn decode_region(
    gray: &[u8],
    width: usize,
    height: usize,
    [a, b, c]: [Finder; 3],
    threshold: u8,
) -> Vec<QrHit> {
    let module = (a.module + b.module + c.module) / 3.;
    let d = (b.x + c.x - a.x, b.y + c.y - a.y);
    let xs = [a.x, b.x, c.x, d.0];
    let ys = [a.y, b.y, c.y, d.1];
    let pad = module * PAD_MODULES + 2.;
    let fold = |v: [f32; 4], f: fn(f32, f32) -> f32| v.into_iter().reduce(f).unwrap();
    let x0 = (fold(xs, f32::min) - pad).floor().max(0.) as usize;
    let y0 = (fold(ys, f32::min) - pad).floor().max(0.) as usize;
    let x1 = ((fold(xs, f32::max) + pad).ceil() as usize).min(width);
    let y1 = ((fold(ys, f32::max) + pad).ceil() as usize).min(height);
    if x1 <= x0 + 20 || y1 <= y0 + 20 {
        return Vec::new();
    }
    let scale = if module < MIN_MODULE_PX {
        (4. / module).ceil().clamp(2., 4.) as usize
    } else {
        1
    };
    // 先按三个定位角实测的深浅色取中值做阈值：整码只看自己的颜色，深底浅码、
    // 低对比和码外背景都不影响；失败再交原灰度给 quirc 在区域内做 Otsu。
    for threshold in [Some(threshold), None] {
        let crop = crop(gray, width, (x0, y0, x1, y1), scale, a.inverted, threshold);
        let hits = quirc(&crop, (x1 - x0) * scale, (y1 - y0) * scale, scale, (x0, y0));
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

/// 定位角中心块（深）与中心外一圈空隙（浅）的平均灰度的中值；深底浅码时两者对调。
fn finder_threshold(gray: &[u8], width: usize, height: usize, finders: [Finder; 3]) -> u8 {
    let at = |x: f32, y: f32| {
        let x = (x.round().max(0.) as usize).min(width - 1);
        let y = (y.round().max(0.) as usize).min(height - 1);
        u32::from(gray[y * width + x])
    };
    let (mut stone, mut gap) = (0, 0);
    for f in finders {
        stone += at(f.x, f.y);
        let r = f.module * 2.;
        gap += (at(f.x - r, f.y) + at(f.x + r, f.y) + at(f.x, f.y - r) + at(f.x, f.y + r)) / 4;
    }
    ((stone + gap) / 6) as u8
}

/// 裁出区域（按需最近邻放大），统一成浅底深码；给了阈值则直接二值化。
fn crop(
    gray: &[u8],
    width: usize,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
    scale: usize,
    inverted: bool,
    threshold: Option<u8>,
) -> Vec<u8> {
    let (w, h) = ((x1 - x0) * scale, (y1 - y0) * scale);
    let mut lut = [0u8; 256];
    for (v, out) in lut.iter_mut().enumerate() {
        let v = v as u8;
        *out = match threshold {
            Some(t) => {
                if (v < t) != inverted {
                    0
                } else {
                    255
                }
            }
            None if inverted => 255 - v,
            None => v,
        };
    }
    let mut out = vec![0u8; w * h];
    for (y, line) in out.chunks_exact_mut(w).enumerate() {
        let src = &gray[(y0 + y / scale) * width + x0..][..x1 - x0];
        for (x, value) in line.iter_mut().enumerate() {
            *value = lut[usize::from(src[x / scale])];
        }
    }
    out
}

/// quirc 解出的码；坐标除以放大倍数再加区域原点，回到整图像素。
pub(super) fn quirc(
    gray: &[u8],
    width: usize,
    height: usize,
    scale: usize,
    (x0, y0): (usize, usize),
) -> Vec<QrHit> {
    let mut decoder = quircs::Quirc::default();
    let mut results = Vec::new();
    let scale = scale as f32;
    for code in decoder.identify(width, height, gray).flatten() {
        let Ok(decoded) = code.decode() else {
            continue;
        };
        let hit = QrHit {
            text: String::from_utf8_lossy(&decoded.payload).into_owned(),
            corners: code.corners.map(|p| {
                (
                    x0 as f32 + p.x as f32 / scale,
                    y0 as f32 + p.y as f32 / scale,
                )
            }),
        };
        push_unique(&mut results, hit);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct Canvas {
        width: usize,
        height: usize,
        gray: Vec<u8>,
    }
    impl Canvas {
        fn new(width: usize, height: usize, value: u8) -> Self {
            Self {
                width,
                height,
                gray: vec![value; width * height],
            }
        }
        fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, value: u8) {
            for row in y..(y + h).min(self.height) {
                let line = &mut self.gray[row * self.width..][..self.width];
                line[x.min(self.width)..(x + w).min(self.width)].fill(value);
            }
        }
        /// 画码（含 4 模块静区底色），返回码区中心。
        fn qr(
            &mut self,
            text: &str,
            left: usize,
            top: usize,
            module: usize,
            dark: u8,
            light: u8,
        ) -> (f32, f32) {
            let code = qrcode::QrCode::new(text.as_bytes()).unwrap();
            let n = code.width();
            self.fill(left, top, (n + 8) * module, (n + 8) * module, light);
            for y in 0..n {
                for x in 0..n {
                    if code[(x, y)] == qrcode::Color::Dark {
                        self.fill(
                            left + (x + 4) * module,
                            top + (y + 4) * module,
                            module,
                            module,
                            dark,
                        );
                    }
                }
            }
            let c = (n + 8) as f32 * module as f32 / 2.;
            (left as f32 + c, top as f32 + c)
        }
        /// 形似定位角的“回”字假目标，模拟文字里抢占 quirc 名额的笔画。
        fn decoy(&mut self, x: usize, y: usize, m: usize) {
            self.fill(x, y, 7 * m, 7 * m, 0);
            self.fill(x + m, y + m, 5 * m, 5 * m, 255);
            self.fill(x + 2 * m, y + 2 * m, 3 * m, 3 * m, 0);
        }
    }

    fn texts(hits: &[QrHit]) -> Vec<String> {
        let mut texts: Vec<_> = hits.iter().map(|h| h.text.clone()).collect();
        texts.sort();
        texts
    }

    #[test]
    fn finds_every_code_in_dense_page_with_finder_like_text() {
        let mut page = Canvas::new(1920, 1080, 255);
        // 页首一排“回”字，整图 quirc 的 32 个定位角名额会先被它们占满。
        for i in 0..60 {
            page.decoy(20 + i * 30, 12, 3);
        }
        let mut expected = Vec::new();
        for row in 0..4 {
            for col in 0..6 {
                let text = format!("https://example.com/{row}/{col}");
                page.qr(
                    &text,
                    30 + col * 310,
                    60 + row * 250,
                    4 + (row + col) % 3,
                    0,
                    255,
                );
                expected.push(text);
            }
        }
        expected.sort();
        let started = Instant::now();
        let hits = scan(&page.gray, page.width, page.height);
        let ours = started.elapsed();
        let started = Instant::now();
        let whole = quirc(&page.gray, page.width, page.height, 1, (0, 0));
        let old = started.elapsed();
        eprintln!(
            "dense page: region scan {} codes in {ours:?}; whole-image quirc {} codes in {old:?}",
            hits.len(),
            whole.len()
        );
        assert_eq!(texts(&hits), expected);
        assert!(whole.len() < expected.len());
    }

    #[test]
    fn finds_inverted_low_contrast_and_tiny_codes_together() {
        let mut page = Canvas::new(1200, 700, 250);
        page.fill(600, 0, 600, 700, 24);
        let a = page.qr("浅底深码", 40, 40, 5, 0, 255);
        // 深色背景上的浅色码（深色模式页面）。
        let b = page.qr("dark mode", 660, 40, 5, 230, 24);
        // 低对比：深灰背景上的灰码。
        let c = page.qr("low contrast", 660, 380, 4, 40, 90);
        // 每模块 2 像素的小码。
        let d = page.qr("tiny", 60, 420, 2, 0, 255);
        let hits = scan(&page.gray, page.width, page.height);
        assert_eq!(
            texts(&hits),
            ["dark mode", "low contrast", "tiny", "浅底深码"]
        );
        for (text, center) in [
            ("浅底深码", a),
            ("dark mode", b),
            ("low contrast", c),
            ("tiny", d),
        ] {
            let hit = hits.iter().find(|h| h.text == text).unwrap();
            let (x, y) = hit.center();
            assert!(
                (x - center.0).abs() < 4. && (y - center.1).abs() < 4.,
                "{hit:?}"
            );
        }
    }

    #[test]
    fn same_text_codes_keep_separate_positions_and_blank_page_is_fast() {
        let mut page = Canvas::new(900, 300, 255);
        page.qr("same", 20, 20, 4, 0, 255);
        page.qr("same", 320, 20, 4, 0, 255);
        page.qr("same", 620, 20, 4, 0, 255);
        assert_eq!(scan(&page.gray, page.width, page.height).len(), 3);

        let blank = Canvas::new(3840, 2160, 255);
        let started = Instant::now();
        assert!(scan(&blank.gray, blank.width, blank.height).is_empty());
        eprintln!("blank 4K: {:?}", started.elapsed());
    }
}
