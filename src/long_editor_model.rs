//! 长图编辑的纯几何：视口约束、总览条映射、分块网格与内容签名。不依赖窗口，便于单测。
//!
//! “长轴”是截图滚动方向（纵向长图为 y），“跨轴”是另一方向。
use crate::{
    geometry::Rect,
    stitch::{ScrollAxis, StitchSnapshot},
};
use std::{
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

/// 分块输出边长（块内物理像素）；块在原图中的跨度为 `TILE << level`。
pub(super) const TILE: u32 = 512;
const MAX_LEVEL: u8 = 12;

/// (x, y) → (长轴, 跨轴)。
pub(super) fn along<T>(axis: ScrollAxis, x: T, y: T) -> (T, T) {
    match axis {
        ScrollAxis::Vertical => (y, x),
        ScrollAxis::Horizontal => (x, y),
    }
}
/// (长轴, 跨轴) → (x, y)。
pub(super) fn compose<T>(axis: ScrollAxis, long: T, cross: T) -> (T, T) {
    match axis {
        ScrollAxis::Vertical => (cross, long),
        ScrollAxis::Horizontal => (long, cross),
    }
}

/// 一维视口约束。`offset` 是原图 0 在窗口里的位置；内容（含两端 `pad` 留白）放得下时居中，
/// 否则只能滚到两端各露出 `pad`，不会把图片拖出可视区。
pub(super) fn clamp_offset(offset: f32, content: f32, start: f32, size: f32, pad: f32) -> f32 {
    if content + 2. * pad <= size {
        start + (size - content) / 2.
    } else {
        offset.clamp(start + size - pad - content, start + pad)
    }
}

/// 选用不低于屏幕分辨率的最粗级别：块像素与屏幕像素之比落在 (0.5, 1]，GPU 只做小幅缩小。
pub(super) fn level_for(zoom: f32) -> u8 {
    if !(zoom > 0.) || zoom >= 1. {
        return 0;
    }
    ((1. / zoom).log2() + 1e-4)
        .floor()
        .clamp(0., MAX_LEVEL as f32) as u8
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct TileKey {
    pub level: u8,
    pub col: u32,
    pub row: u32,
    /// 与块相交的条带及其相对位置；删段、撤销后内容未变的块签名不变，可直接复用。
    pub sig: u64,
}

pub(super) fn tile_rect(level: u8, col: u32, row: u32, width: u32, height: u32) -> Option<Rect> {
    let span = (TILE as u64) << level;
    let x = col as u64 * span;
    let y = row as u64 * span;
    if x >= width as u64 || y >= height as u64 {
        return None;
    }
    Some(Rect {
        x: x as u32,
        y: y as u32,
        width: span.min(width as u64 - x) as u32,
        height: span.min(height as u64 - y) as u32,
    })
}

pub(super) fn tile_output(level: u8, rect: Rect) -> (u32, u32) {
    let step = 1u32 << level;
    (
        rect.width.div_ceil(step).max(1),
        rect.height.div_ceil(step).max(1),
    )
}

/// 覆盖 `visible`（原图坐标，已裁到图内）的块列、行范围。
pub(super) fn tile_range(level: u8, visible: Rect) -> (Range<u32>, Range<u32>) {
    let span = (TILE as u64) << level;
    let cols = (visible.x as u64 / span) as u32
        ..((visible.x as u64 + visible.width as u64).div_ceil(span)) as u32;
    let rows = (visible.y as u64 / span) as u32
        ..((visible.y as u64 + visible.height as u64).div_ceil(span)) as u32;
    (cols, rows)
}

pub(super) fn signature(snapshot: &StitchSnapshot, rect: Rect) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (rect.width, rect.height).hash(&mut hasher);
    for segment in &snapshot.segments {
        let placed = Rect {
            x: segment.origin.0,
            y: segment.origin.1,
            width: segment.source.width,
            height: segment.source.height,
        };
        if placed.intersection(rect).is_none() {
            continue;
        }
        (
            Arc::as_ptr(&segment.image) as usize,
            segment.source.x,
            segment.source.y,
            segment.source.width,
            segment.source.height,
            segment.origin.0 as i64 - rect.x as i64,
            segment.origin.1 as i64 - rect.y as i64,
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// 右侧（横向长图在下方）总览条。内容按跨轴宽度缩放；比面板长时像滚动条一样随视口滚动，
/// 指示框位置与视口起点成正比，拖动指示框即可直接换算视口。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Minimap {
    /// 原图 → 总览物理像素。
    pub zoom: f32,
    /// 总览内容沿长轴的滚动量。
    pub scroll: f32,
    /// 视口起点每移动 1 原图像素，指示框移动多少。
    pub slope: f32,
    /// 指示框沿长轴的起点与长度（相对面板可用区起点）。
    pub indicator: (f32, f32),
}
impl Minimap {
    pub(super) fn new(
        doc_long: f32,
        doc_cross: f32,
        panel_long: f32,
        panel_cross: f32,
        view_start: f32,
        view_len: f32,
    ) -> Self {
        let zoom = (panel_cross / doc_cross.max(1.)).clamp(1e-6, 1.);
        let content = doc_long * zoom;
        let view_len = view_len.clamp(0., doc_long);
        let travel = (doc_long - view_len).max(0.);
        let view_start = view_start.clamp(0., travel);
        let (scroll, slope) = if content <= panel_long || travel <= 0. {
            (0., zoom)
        } else {
            (
                (content - panel_long) * view_start / travel,
                (panel_long - view_len * zoom) / travel,
            )
        };
        Self {
            zoom,
            scroll,
            slope,
            indicator: (view_start * zoom - scroll, view_len * zoom),
        }
    }
    /// 面板长轴位置对应的原图长轴坐标。
    pub(super) fn image_at(self, panel: f32) -> f32 {
        (panel + self.scroll) / self.zoom
    }
    /// 指示框起点拖到 `indicator_start` 时视口应从原图哪里开始。
    pub(super) fn view_start_for(self, indicator_start: f32) -> f32 {
        indicator_start / self.slope.max(1e-6)
    }
}

/// 删除 [start, end) 后，原长轴坐标的新位置；落在删除段内的收到段首。
pub(super) fn after_cut(position: f32, start: u32, end: u32) -> f32 {
    if position <= start as f32 {
        position
    } else if position < end as f32 {
        start as f32
    } else {
        position - (end - start) as f32
    }
}

/// 两次长轴位置组成的删除段（取整、限在图内），空段返回 None。
pub(super) fn band(a: f32, b: f32, long: u32) -> Option<(u32, u32)> {
    let clamp = |v: f32| v.round().clamp(0., long as f32) as u32;
    let (lo, hi) = (clamp(a.min(b)), clamp(a.max(b)));
    (hi > lo).then_some((lo, hi))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Side {
    Left,
    Top,
    Right,
    Bottom,
}
impl Side {
    /// 是否沿长轴（裁掉开头或结尾），拖动时需要自动滚动。
    pub(super) fn is_long(self, axis: ScrollAxis) -> bool {
        matches!(
            (axis, self),
            (ScrollAxis::Vertical, Side::Top | Side::Bottom)
                | (ScrollAxis::Horizontal, Side::Left | Side::Right)
        )
    }
}

/// 把 `side` 这条边拖到原图坐标 `at` 后保留的范围；至少留 1 像素。
pub(super) fn trimmed(width: u32, height: u32, side: Side, at: f32) -> Rect {
    let at = at.round().max(0.) as u32;
    match side {
        Side::Left => {
            let x = at.min(width - 1);
            Rect {
                x,
                y: 0,
                width: width - x,
                height,
            }
        }
        Side::Right => Rect {
            x: 0,
            y: 0,
            width: at.clamp(1, width),
            height,
        },
        Side::Top => {
            let y = at.min(height - 1);
            Rect {
                x: 0,
                y,
                width,
                height: height - y,
            }
        }
        Side::Bottom => Rect {
            x: 0,
            y: 0,
            width,
            height: at.clamp(1, height),
        },
    }
}

/// 千位分隔，状态栏显示像素数。
pub(super) fn grouped(value: u32) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in text.chars().enumerate() {
        if index > 0 && (text.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture::Image, stitch::StitchSegment};

    fn snapshot(height: u32, strip: u32) -> StitchSnapshot {
        let mut segments = Vec::new();
        let mut y = 0;
        while y < height {
            let h = strip.min(height - y);
            segments.push(StitchSegment {
                image: Arc::new(Image {
                    width: 64,
                    height: h,
                    bgra: vec![0; 64 * h as usize * 4],
                    readback_ms: 0.,
                }),
                source: Rect::full(64, h),
                origin: (0, y),
            });
            y += h;
        }
        StitchSnapshot {
            width: 64,
            height,
            axis: ScrollAxis::Vertical,
            viewport: Rect::full(64, strip.min(height)),
            segments,
        }
    }

    /// 手动测耗时：`cargo test --lib long_editor::model::tests::tile_cost -- --ignored --nocapture`。
    #[test]
    #[ignore]
    fn tile_cost() {
        // 接近真实采集：每条带引用一整帧 1920×1080，只取其中 120 行。
        let frame = Arc::new(Image {
            width: 1920,
            height: 1080,
            bgra: (0..1920 * 1080 * 4).map(|i| (i % 251) as u8).collect(),
            readback_ms: 0.,
        });
        let (width, height, strip) = (1920u32, 24_000u32, 120u32);
        let segments: Vec<_> = (0..height / strip)
            .map(|i| StitchSegment {
                image: frame.clone(),
                source: Rect {
                    x: 0,
                    y: 480,
                    width,
                    height: strip,
                },
                origin: (0, i * strip),
            })
            .collect();
        let snapshot = StitchSnapshot {
            width,
            height,
            axis: ScrollAxis::Vertical,
            viewport: Rect::full(width, 1080),
            segments,
        };
        let t = std::time::Instant::now();
        for i in 0..60 {
            let r = tile_rect(0, i % 4, i / 4, width, height).unwrap();
            std::hint::black_box(signature(&snapshot, r));
        }
        eprintln!("signature x60: {:?}", t.elapsed());
        for level in 0..5u8 {
            let r = tile_rect(level, 0, 1, width, height).unwrap();
            let (w, h) = tile_output(level, r);
            let t = std::time::Instant::now();
            let image = if level == 0 {
                crate::long_document::render_region(&snapshot, r, w, h)
            } else {
                crate::long_document::render_preview_region(&snapshot, r, w, h)
            }
            .unwrap();
            eprintln!("level {level} {r:?} -> {w}x{h}: {:?}", t.elapsed());
            std::hint::black_box(image);
        }
    }

    #[test]
    fn clamp_centers_small_content_and_bounds_large_content() {
        assert_eq!(clamp_offset(-500., 100., 10., 400., 20.), 160.);
        // 大图两端各最多露出 pad。
        assert_eq!(clamp_offset(500., 1000., 10., 400., 20.), 30.);
        assert_eq!(clamp_offset(-5000., 1000., 10., 400., 20.), -610.);
        assert_eq!(clamp_offset(-100., 1000., 10., 400., 20.), -100.);
    }

    #[test]
    fn level_keeps_tile_pixels_at_or_above_screen_resolution() {
        assert_eq!(level_for(2.), 0);
        assert_eq!(level_for(1.), 0);
        assert_eq!(level_for(0.6), 0);
        assert_eq!(level_for(0.5), 1);
        assert_eq!(level_for(0.26), 1);
        assert_eq!(level_for(0.25), 2);
        assert_eq!(level_for(0.01), 6);
        assert_eq!(level_for(f32::NAN), 0);
        for zoom in [0.9f32, 0.5, 0.33, 0.12, 0.031] {
            let scale = 1. / (1u32 << level_for(zoom)) as f32;
            assert!(scale >= zoom * 0.999 && scale < zoom * 2.001, "{zoom}");
        }
    }

    #[test]
    fn tiles_cover_document_exactly() {
        let (w, h) = (1300, 2600);
        for level in 0..3u8 {
            let (cols, rows) = tile_range(level, Rect::full(w, h));
            let mut area = 0u64;
            for row in rows.clone() {
                for col in cols.clone() {
                    let r = tile_rect(level, col, row, w, h).expect("范围内的块");
                    area += r.width as u64 * r.height as u64;
                    let (ow, oh) = tile_output(level, r);
                    assert!(ow <= TILE && oh <= TILE);
                }
            }
            assert_eq!(area, w as u64 * h as u64);
            assert!(tile_rect(level, cols.end, 0, w, h).is_none());
        }
        let (cols, rows) = tile_range(
            0,
            Rect {
                x: 511,
                y: 1024,
                width: 2,
                height: 1,
            },
        );
        assert_eq!((cols, rows), (0..2, 2..3));
    }

    #[test]
    fn signature_survives_cut_below_and_changes_when_content_moves() {
        let mut document = crate::long_document::LongDocument::new(snapshot(4096, 1024)).unwrap();
        let top = Rect::full(64, 512);
        let lower = Rect {
            x: 0,
            y: 2048,
            width: 64,
            height: 512,
        };
        let before = (
            signature(&document.snapshot(), top),
            signature(&document.snapshot(), lower),
        );
        assert!(document.cut(1500, 1700).unwrap());
        let after = document.snapshot();
        assert_eq!(signature(&after, top), before.0, "删段之上的块应复用");
        assert_ne!(signature(&after, lower), before.1, "删段之后的内容已上移");
        assert!(document.undo());
        assert_eq!(
            signature(&document.snapshot(), lower),
            before.1,
            "撤销后复用旧块"
        );
    }

    #[test]
    fn minimap_indicator_moves_like_a_scrollbar() {
        // 20000 长、1000 宽的图，总览 100 宽、600 长；视口 2000 原图像素。
        let at = |start| Minimap::new(20_000., 1000., 600., 100., start, 2000.);
        let first = at(0.);
        assert_eq!(first.zoom, 0.1);
        assert_eq!(first.indicator, (0., 200.));
        let last = at(18_000.);
        assert!((last.indicator.0 + last.indicator.1 - 600.).abs() < 0.01);
        let middle = at(9000.);
        assert!((middle.view_start_for(middle.indicator.0) - 9000.).abs() < 0.1);
        assert!((middle.image_at(middle.indicator.0) - 9000.).abs() < 0.1);
        // 短图不滚动，指示框直接按比例。
        let short = Minimap::new(3000., 1000., 600., 100., 500., 1000.);
        assert_eq!((short.scroll, short.indicator), (0., (50., 100.)));
    }

    #[test]
    fn cut_band_and_trim_geometry() {
        assert_eq!(after_cut(10., 100, 300), 10.);
        assert_eq!(after_cut(150., 100, 300), 100.);
        assert_eq!(after_cut(500., 100, 300), 300.);
        assert_eq!(band(80.4, 20.6, 50), Some((21, 50)));
        assert_eq!(band(10., 10.2, 50), None);
        assert_eq!(band(-5., 3., 50), Some((0, 3)));
        assert_eq!(
            trimmed(640, 480, Side::Top, 40.),
            Rect {
                x: 0,
                y: 40,
                width: 640,
                height: 440
            }
        );
        assert_eq!(trimmed(640, 480, Side::Right, 9000.), Rect::full(640, 480));
        assert_eq!(trimmed(640, 480, Side::Left, 9000.).width, 1);
        assert_eq!(trimmed(640, 480, Side::Bottom, -3.).height, 1);
        assert!(Side::Top.is_long(ScrollAxis::Vertical));
        assert!(!Side::Top.is_long(ScrollAxis::Horizontal));
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(grouped(999), "999");
    }
}
