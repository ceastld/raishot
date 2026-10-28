//! 长截图实时预览的固定比例缩略线缓存。
//!
//! 缩略图按跨轴比例建立全局网格，长轴每条线对应长图中固定的一段源像素。新拼接的内容只渲染
//! 新增或变化的线，投影窗口滑动时复用已渲染的线；发布时只按行拷贝拼成当前窗口的小图。
use crate::{
    capture::Image,
    geometry::Rect,
    stitch::{ScrollAxis, StitchSnapshot},
    Result,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grid {
    axis: ScrollAxis,
    source: u32,
    dest: u32,
}

/// 条带身份：同一 Arc 图像、同一取样矩形、同一长轴起点即像素相同。Weak 防止旧地址被复用。
struct SegmentKey {
    image: Weak<Image>,
    source: Rect,
    origin: u32,
}
impl SegmentKey {
    fn same(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.image, &other.image)
            && self.source == other.source
            && self.origin == other.origin
    }
}

#[derive(Default)]
pub(crate) struct PreviewStrip {
    grid: Option<Grid>,
    first: u32,
    lines: VecDeque<Vec<u8>>,
    segments: Vec<SegmentKey>,
    /// 累计实际渲染的缩略线数，只用于确认增量路径。
    rendered_lines: u64,
}

impl PreviewStrip {
    #[cfg(test)]
    pub(crate) fn rendered_lines(&self) -> u64 {
        self.rendered_lines
    }

    /// 生成 `window`（长图坐标）的缩略图。`max_cross`/`max_long` 是缩略图跨轴/长轴像素上限，
    /// `cap` 是投影窗口的最大长轴源长度；三者决定本代固定不变的缩放比例。
    pub(crate) fn render(
        &mut self,
        snapshot: &StitchSnapshot,
        max_cross: u32,
        max_long: u32,
        cap: u32,
        window: Rect,
    ) -> Result<(Image, Rect)> {
        let axis = snapshot.axis;
        let (cross, long) = match axis {
            ScrollAxis::Vertical => (snapshot.width, snapshot.height),
            ScrollAxis::Horizontal => (snapshot.height, snapshot.width),
        };
        let (window_start, window_length) = match axis {
            ScrollAxis::Vertical => (window.y, window.height),
            ScrollAxis::Horizontal => (window.x, window.width),
        };
        if cross == 0 || long == 0 || max_cross == 0 || max_long == 0 || window_length == 0 {
            return Err("长截图预览尺寸无效".into());
        }
        let scale = (max_cross as f64 / cross as f64)
            .min(max_long as f64 / cap.max(1) as f64)
            .min(1.);
        let grid = Grid {
            axis,
            source: cross,
            dest: ((cross as f64 * scale).round() as u32).clamp(1, cross),
        };
        if self.grid != Some(grid) {
            self.grid = Some(grid);
            self.lines.clear();
            self.segments.clear();
        }
        let (source, dest) = (grid.source as u64, grid.dest as u64);
        let to_line = |position: u32| (position as u64 * dest / source) as u32;
        let total = (long as u64 * dest).div_ceil(source) as u32;

        // 与上次渲染比较条带：首个不同条带之前的长图内容未变，之后的线需要重绘。
        let mut keys: Vec<SegmentKey> = snapshot
            .segments
            .iter()
            .map(|segment| SegmentKey {
                image: Arc::downgrade(&segment.image),
                source: segment.source,
                origin: match axis {
                    ScrollAxis::Vertical => segment.origin.1,
                    ScrollAxis::Horizontal => segment.origin.0,
                },
            })
            .collect();
        keys.sort_by_key(|key| key.origin);
        let common = self
            .segments
            .iter()
            .zip(&keys)
            .take_while(|(a, b)| a.same(b))
            .count();
        let dirty_from = match (self.segments.get(common), keys.get(common)) {
            (Some(a), Some(b)) => Some(a.origin.min(b.origin)),
            (Some(k), None) | (None, Some(k)) => Some(k.origin),
            (None, None) => None,
        };
        let mut keep = total;
        if let Some(position) = dirty_from {
            keep = keep.min(to_line(position));
        }
        // 末条可能只部分覆盖；长图在其后增长时，它随新条带一起失效重绘。
        let end = self.first as u64 + self.lines.len() as u64;
        if end > keep as u64 {
            let retained = (keep as u64).saturating_sub(self.first as u64) as usize;
            self.lines.truncate(retained);
        }
        self.segments = keys;

        let mut first = (window_start as u64 * dest).div_ceil(source) as u32;
        // 窗口到达长图末端时包含部分覆盖的末条，其余位置只取完整覆盖的线。
        let window_end = window_start.saturating_add(window_length);
        let mut last = if window_end >= long {
            total
        } else {
            to_line(window_end).min(total)
        };
        if last <= first {
            last = (first + 1).min(total);
            first = last.saturating_sub(1);
        }
        if total == 0 || last <= first {
            return Err("长截图预览没有可显示的内容".into());
        }

        let end = self.first + self.lines.len() as u32;
        if self.lines.is_empty() || first > end || last < self.first {
            self.lines.clear();
            self.first = first;
        }
        if first < self.first {
            let rendered = crate::long_document::render_preview_lines(
                snapshot,
                grid.source,
                grid.dest,
                first..self.first,
            )?;
            self.rendered_lines += rendered.len() as u64;
            for line in rendered.into_iter().rev() {
                self.lines.push_front(line);
            }
            self.first = first;
        }
        let end = self.first + self.lines.len() as u32;
        if last > end {
            let rendered = crate::long_document::render_preview_lines(
                snapshot,
                grid.source,
                grid.dest,
                end..last,
            )?;
            self.rendered_lines += rendered.len() as u64;
            self.lines.extend(rendered);
        }
        // 只在当前窗口两侧各留半个窗口，回滚时复用，内存与窗口同阶。
        let margin = ((last - first) / 2).max(16);
        while self.first.saturating_add(margin) < first && !self.lines.is_empty() {
            self.lines.pop_front();
            self.first += 1;
        }
        while self.first + self.lines.len() as u32 > last.saturating_add(margin) {
            self.lines.pop_back();
        }

        let count = (last - first) as usize;
        let line_bytes = grid.dest as usize * 4;
        let offset = (first - self.first) as usize;
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(count * line_bytes)?;
        let (width, height) = match axis {
            ScrollAxis::Vertical => {
                for line in self.lines.range(offset..offset + count) {
                    bgra.extend_from_slice(line);
                }
                (grid.dest, count as u32)
            }
            ScrollAxis::Horizontal => {
                // 缓存按列存放；发布时转置成行优先的小图。
                for y in 0..grid.dest as usize {
                    for line in self.lines.range(offset..offset + count) {
                        bgra.extend_from_slice(&line[y * 4..y * 4 + 4]);
                    }
                }
                (count as u32, grid.dest)
            }
        };
        let start = ((first as u64 * source + dest / 2) / dest).min(long as u64 - 1) as u32;
        let stop = ((last as u64 * source + dest / 2) / dest)
            .min(long as u64)
            .max(start as u64 + 1) as u32;
        Ok((
            Image {
                width,
                height,
                bgra,
                readback_ms: 0.,
            },
            axis.rect(start, stop - start, cross),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stitch::StitchSegment;

    fn image(width: u32, height: u32, seed: u32) -> Arc<Image> {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let v = (x * 7 + y * 13 + seed * 31) as u8;
                bgra.extend_from_slice(&[v, v.wrapping_mul(3), v ^ 0x5a, 255]);
            }
        }
        Arc::new(Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        })
    }

    /// 由若干等长条带组成的长图；`frames` 共享，前缀条带保持同一 Arc。
    fn snapshot(axis: ScrollAxis, frames: &[Arc<Image>], strip: u32) -> StitchSnapshot {
        let (cross, frame_long) = axis.size(&frames[0]);
        let mut segments = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let (start, length) = if index == 0 {
                (0, frame_long)
            } else {
                (frame_long - strip, strip)
            };
            let origin = if index == 0 {
                0
            } else {
                frame_long + (index as u32 - 1) * strip
            };
            segments.push(StitchSegment {
                image: frame.clone(),
                source: axis.rect(start, length, cross),
                origin: axis.origin(origin),
            });
        }
        let long = frame_long + (frames.len() as u32 - 1) * strip;
        let (width, height) = axis.dimensions(cross, long);
        StitchSnapshot {
            width,
            height,
            axis,
            viewport: axis.rect(long - frame_long, frame_long, cross),
            segments,
        }
    }

    #[test]
    fn long_preview_strip_growth_renders_only_new_lines_and_matches_fresh_render() {
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let frames: Vec<_> = (0..12)
                .map(|seed| match axis {
                    ScrollAxis::Vertical => image(400, 300, seed),
                    ScrollAxis::Horizontal => image(300, 400, seed),
                })
                .collect();
            let mut strip = PreviewStrip::default();
            let mut previous_rendered = 0;
            for count in 1..=frames.len() {
                let document = snapshot(axis, &frames[..count], 40);
                let long = match axis {
                    ScrollAxis::Vertical => document.height,
                    ScrollAxis::Horizontal => document.width,
                };
                let window = axis.rect(0, long, 400);
                let (image, source) = strip.render(&document, 250, 875, 1400, window).unwrap();
                let fresh = PreviewStrip::default()
                    .render(&document, 250, 875, 1400, window)
                    .unwrap();
                // 增量结果与从零渲染逐字节一致。
                assert_eq!(image.bgra, fresh.0.bgra);
                assert_eq!((image.width, image.height), (fresh.0.width, fresh.0.height));
                assert_eq!(source, fresh.1);
                let lines = (long as u64 * 250).div_ceil(400);
                let cross = match axis {
                    ScrollAxis::Vertical => (image.width, image.height as u64),
                    ScrollAxis::Horizontal => (image.height, image.width as u64),
                };
                assert_eq!(cross, (250, lines));
                // 每追加 40px 只重绘约 25 条新线及原末条，不重绘整图。
                if count > 1 {
                    assert!(strip.rendered_lines() - previous_rendered <= 27);
                }
                previous_rendered = strip.rendered_lines();
            }
        }
    }

    #[test]
    fn long_preview_strip_window_slide_reuses_lines_and_replaced_tail_redraws() {
        let frames: Vec<_> = (0..40).map(|seed| image(400, 300, seed)).collect();
        let document = snapshot(ScrollAxis::Vertical, &frames, 40);
        let mut strip = PreviewStrip::default();
        let window = |y| Rect {
            x: 0,
            y,
            width: 400,
            height: 800,
        };
        strip.render(&document, 250, 875, 1400, window(0)).unwrap();
        let initial = strip.rendered_lines();
        // 窗口下移 80px：只渲染新进入的 50 条线。
        let (slid, source) = strip.render(&document, 250, 875, 1400, window(80)).unwrap();
        assert_eq!(strip.rendered_lines() - initial, 50);
        let fresh = PreviewStrip::default()
            .render(&document, 250, 875, 1400, window(80))
            .unwrap();
        assert_eq!((slid.bgra, source), (fresh.0.bgra, fresh.1));

        // 替换末尾条带（例如固定页脚或撤销后的新内容）：只有其后的线重绘，结果与从零一致。
        let mut replaced = frames.clone();
        *replaced.last_mut().unwrap() = image(400, 300, 99);
        let changed = snapshot(ScrollAxis::Vertical, &replaced, 40);
        let tail = Rect {
            x: 0,
            y: changed.height - 800,
            width: 400,
            height: 800,
        };
        strip.render(&document, 250, 875, 1400, tail).unwrap();
        let before = strip.rendered_lines();
        let (redrawn, _) = strip.render(&changed, 250, 875, 1400, tail).unwrap();
        assert!(strip.rendered_lines() - before <= 26);
        let fresh = PreviewStrip::default()
            .render(&changed, 250, 875, 1400, tail)
            .unwrap();
        assert_eq!(redrawn.bgra, fresh.0.bgra);
    }
}
