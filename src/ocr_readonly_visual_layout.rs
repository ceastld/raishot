//! 只在 StyledText 后的当前 paint 阶段读取 GPUI 实际字形及软折行。
use super::readonly_visual_navigation::{VisualLayout, VisualLine, VisualStop};
use gpui::TextLayout;
use unicode_segmentation::UnicodeSegmentation;

const MAX_BODY_BYTES: usize = 65_536;
// 公共 line_layout_for_index 从第一硬段查起；先限制段数，避免另一个平方扫描。
const MAX_HARD_LINES: usize = 512;
const MAX_TOTAL_GLYPHS: usize = 262_144;
const MAX_VISUAL_ROWS: usize = 65_536;

fn navigation_body_within_budget(text: &str) -> bool {
    text.len() <= MAX_BODY_BYTES
        && text.bytes().filter(|byte| *byte == b'\n').count() < MAX_HARD_LINES
}

/// queries 必须是原文的递增完整字素 byte。保留 GPUI 的首个合格 glyph 语义：
/// 原迭代顺序中第一个 index >= query 的 x，否则用 width；不排序 glyph。
/// 递增 query 只会淘汰此前不再合格的 glyph，非单调/重复 index 也不需回扫。
fn collect_stop_positions(
    queries: &[usize],
    hard_start: usize,
    hard_len: usize,
    glyphs: impl Iterator<Item = (usize, f32)>,
    width: f32,
    glyph_budget: &mut usize,
) -> Option<Vec<VisualStop>> {
    if !width.is_finite() {
        return None;
    }
    let mut glyphs = glyphs.peekable();
    let mut stops = Vec::with_capacity(queries.len());
    let mut previous = None;
    for &byte in queries {
        let query = byte.checked_sub(hard_start)?;
        if query > hard_len || previous.is_some_and(|previous| byte <= previous) {
            return None;
        }
        previous = Some(byte);
        while let Some(&(index, x)) = glyphs.peek() {
            if index > hard_len || !x.is_finite() {
                return None;
            }
            if index >= query {
                break;
            }
            glyphs.next();
            *glyph_budget = glyph_budget.checked_sub(1)?;
        }
        let x = glyphs.peek().map_or(width, |(_, x)| *x);
        stops.push(VisualStop { byte, x });
    }
    // 尾部也只访问一次；未命中的非法 glyph 或总预算耗尽同样安全拒绝。
    for (index, x) in glyphs {
        *glyph_budget = glyph_budget.checked_sub(1)?;
        if index > hard_len || !x.is_finite() {
            return None;
        }
    }
    Some(stops)
}

fn append_soft_rows(
    rows: &mut Vec<VisualLine>,
    stops: &[VisualStop],
    soft_ends: &[usize],
    hard_start: usize,
    hard_len: usize,
) -> Option<()> {
    let mut soft_start = 0;
    for &soft_end in soft_ends {
        if soft_end < soft_start || soft_end > hard_len || rows.len() >= MAX_VISUAL_ROWS {
            return None;
        }
        let first_byte = hard_start.checked_add(soft_start)?;
        let last_byte = hard_start.checked_add(soft_end)?;
        let first = stops.partition_point(|stop| stop.byte < first_byte);
        let last = stops.partition_point(|stop| stop.byte <= last_byte);
        let start = stops.get(first)?;
        if start.byte != first_byte {
            return None;
        }
        let row_stops = stops[first..last]
            .iter()
            .map(|stop| VisualStop {
                byte: stop.byte,
                x: stop.x - start.x,
            })
            .collect();
        // 共享的 soft_end 在前行末与后行首各存一次，VerticalState 保存行归属。
        // CRLF 中间的 byte 不在整篇字素表中；连续/末尾 LF 的空硬段仍保留一行。
        rows.push(VisualLine { stops: row_stops });
        soft_start = soft_end;
    }
    Some(())
}

/// 调用方必须证明本 TextLayout 已完成 measurement/prepaint，并属于当前 render。
/// 行来自 GPUI 的 WrappedLineLayout；硬段只用于对应其 split('\n') 的原文字节偏移。
/// 不能在按键时调用未 paint 的 TextLayout，也不能拿硬换行代替软行。
pub(crate) fn from_painted_layout(text: &str, layout: &TextLayout) -> Option<VisualLayout> {
    // 无法安全/有界提取时由现有宿主 publish_without_rows 保留鼠标帧，不造停点。
    if !navigation_body_within_budget(text) {
        return None;
    }
    let line_height = f32::from(layout.line_height());
    if layout.len() != text.len() || !line_height.is_finite() || line_height <= 0. {
        return None;
    }
    let boundaries: Vec<usize> = text
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .chain([text.len()])
        .collect();
    let mut rows = Vec::new();
    let mut hard_start = 0;
    let mut glyph_budget = MAX_TOTAL_GLYPHS;
    for segment in text.split('\n') {
        let shaped = layout.line_layout_for_index(hard_start)?;
        if shaped.len() != segment.len() || shaped.wrap_boundaries().len() >= MAX_VISUAL_ROWS {
            return None;
        }
        let mut soft_ends = Vec::with_capacity(shaped.wrap_boundaries().len() + 1);
        for boundary in shaped.wrap_boundaries() {
            let glyph = shaped
                .unwrapped_layout
                .runs
                .get(boundary.run_ix)?
                .glyphs
                .get(boundary.glyph_ix)?;
            // 折行若恰落在字素内部，不伪造可导航的完整字素行。
            if boundaries
                .binary_search(&hard_start.checked_add(glyph.index)?)
                .is_err()
            {
                return None;
            }
            soft_ends.push(glyph.index);
        }
        soft_ends.push(segment.len());
        let hard_end = hard_start.checked_add(segment.len())?;
        let first = boundaries.partition_point(|byte| *byte < hard_start);
        let last = boundaries.partition_point(|byte| *byte <= hard_end);
        let stops = collect_stop_positions(
            &boundaries[first..last],
            hard_start,
            segment.len(),
            shaped
                .unwrapped_layout
                .runs
                .iter()
                .flat_map(|run| &run.glyphs)
                .map(|glyph| (glyph.index, f32::from(glyph.position.x))),
            f32::from(shaped.unwrapped_layout.width),
            &mut glyph_budget,
        )?;
        append_soft_rows(&mut rows, &stops, &soft_ends, hard_start, segment.len())?;
        hard_start = hard_end.checked_add(1)?;
    }
    VisualLayout::new(text, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn linear_positions_preserve_first_qualifying_mixed_direction_and_repeated_bytes() {
        let mut budget = 5;
        let stops = collect_stop_positions(
            &[0, 1, 3, 5, 6, 10, 11],
            0,
            11,
            [(5, 50.), (3, 30.), (5, 500.), (10, 100.), (8, 80.)].into_iter(),
            110.,
            &mut budget,
        )
        .unwrap();
        assert_eq!(
            stops.iter().map(|stop| stop.x).collect::<Vec<_>>(),
            [50., 50., 50., 50., 100., 100., 110.]
        );
        assert_eq!(budget, 0);
    }

    #[test]
    fn linear_positions_keep_original_cluster_queries_and_ligature_gaps() {
        let text = "a\u{301}👩‍👩‍👧‍👦中";
        let original = text.as_bytes().to_vec();
        let queries: Vec<_> = text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain([text.len()])
            .collect();
        let chinese = text.find('中').unwrap();
        let mut budget = 3;
        let stops = collect_stop_positions(
            &queries,
            0,
            text.len(),
            [(0, 0.), (1, 4.), (chinese, 27.)].into_iter(),
            35.,
            &mut budget,
        )
        .unwrap();
        assert_eq!(
            stops.iter().map(|stop| stop.byte).collect::<Vec<_>>(),
            queries
        );
        assert_eq!(
            stops.iter().map(|stop| stop.x).collect::<Vec<_>>(),
            [0., 27., 27., 35.]
        );
        assert_eq!(text.as_bytes(), original);
        assert!(VisualLayout::new(text, vec![VisualLine { stops }]).is_some());
    }

    #[test]
    fn linear_soft_rows_duplicate_shared_stop_without_another_glyph_scan() {
        let mut budget = 4;
        let stops = collect_stop_positions(
            &[0, 1, 2, 3, 4],
            0,
            4,
            [(0, 10.), (1, 20.), (2, 30.), (3, 40.)].into_iter(),
            50.,
            &mut budget,
        )
        .unwrap();
        let mut rows = Vec::new();
        append_soft_rows(&mut rows, &stops, &[2, 4], 0, 4).unwrap();
        assert_eq!(
            rows[0].stops.iter().map(|s| s.byte).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            rows[1].stops.iter().map(|s| s.byte).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        for row in &rows {
            assert_eq!(
                row.stops.iter().map(|s| s.x).collect::<Vec<_>>(),
                [0., 10., 20.]
            );
        }
        assert!(VisualLayout::new("abcd", rows).is_some());
        assert!(append_soft_rows(&mut Vec::new(), &stops, &[3, 2, 4], 0, 4).is_none());
        assert!(append_soft_rows(&mut Vec::new(), &stops[2..], &[4], 0, 4).is_none());
    }

    #[test]
    fn linear_crlf_and_terminal_empty_segments_keep_exact_original_boundaries() {
        let text = "a\r\n\n";
        let queries: Vec<_> = text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain([text.len()])
            .collect();
        assert_eq!(queries, [0, 1, 3, 4]);
        let mut rows = Vec::new();
        let mut budget = 2;
        let stops = collect_stop_positions(
            &queries[..2],
            0,
            2,
            [(0, 7.), (1, 12.)].into_iter(),
            15.,
            &mut budget,
        )
        .unwrap();
        append_soft_rows(&mut rows, &stops, &[2], 0, 2).unwrap();
        for byte in [3, 4] {
            let stops =
                collect_stop_positions(&[byte], byte, 0, std::iter::empty(), 0., &mut budget)
                    .unwrap();
            append_soft_rows(&mut rows, &stops, &[0], byte, 0).unwrap();
        }
        assert_eq!(
            rows.iter()
                .flat_map(|row| row.stops.iter().map(|s| s.byte))
                .collect::<Vec<_>>(),
            queries
        );
        assert_eq!(rows[0].stops[1].x, 5.);
        assert_eq!(rows[1].stops, [VisualStop { byte: 3, x: 0. }]);
        assert_eq!(rows[2].stops, [VisualStop { byte: 4, x: 0. }]);
        assert!(VisualLayout::new(text, rows).is_some());
    }

    #[test]
    fn linear_positions_visit_each_glyph_once_for_long_ascii() {
        let count = 10_000;
        let visits = Cell::new(0);
        let queries: Vec<_> = (0..=count).collect();
        let glyphs = (0..count).map(|index| {
            visits.set(visits.get() + 1);
            (index, index as f32)
        });
        let mut budget = count;
        let stops =
            collect_stop_positions(&queries, 0, count, glyphs, count as f32, &mut budget).unwrap();
        assert_eq!(visits.get(), count);
        assert_eq!(budget, 0);
        assert_eq!(stops.len(), count + 1);
        for (index, stop) in stops.iter().enumerate() {
            assert_eq!(stop.byte, index);
            assert_eq!(stop.x, index as f32);
        }
    }

    #[test]
    fn linear_positions_reject_invalid_queries_glyphs_and_exhausted_budget() {
        for glyph in [(4, 0.), (0, f32::NAN), (0, f32::INFINITY)] {
            let mut budget = 1;
            assert!(
                collect_stop_positions(&[0, 3], 0, 3, [glyph].into_iter(), 5., &mut budget)
                    .is_none()
            );
        }
        let mut budget = 1;
        assert!(
            collect_stop_positions(&[2, 1], 0, 3, std::iter::empty(), 5., &mut budget).is_none()
        );
        assert!(collect_stop_positions(&[0], 1, 3, std::iter::empty(), 5., &mut budget).is_none());
        assert!(collect_stop_positions(&[4], 0, 3, std::iter::empty(), 5., &mut budget).is_none());
        assert!(
            collect_stop_positions(&[0], 0, 0, std::iter::empty(), f32::NAN, &mut budget).is_none()
        );
        let mut budget = 0;
        assert!(
            collect_stop_positions(&[0], 0, 3, [(2, 5.)].into_iter(), 9., &mut budget).is_none()
        );
        let mut budget = 2;
        assert!(collect_stop_positions(
            &[0],
            0,
            3,
            [(3, 5.), (4, 9.)].into_iter(),
            10.,
            &mut budget
        )
        .is_none());
    }

    #[test]
    fn linear_hard_line_budget_is_checked_before_any_layout_lookup() {
        assert!(navigation_body_within_budget(""));
        assert!(navigation_body_within_budget(
            &"\n".repeat(MAX_HARD_LINES - 1)
        ));
        assert!(!navigation_body_within_budget(&"\n".repeat(MAX_HARD_LINES)));
        assert!(navigation_body_within_budget(&"a".repeat(MAX_BODY_BYTES)));
        assert!(!navigation_body_within_budget(
            &"a".repeat(MAX_BODY_BYTES + 1)
        ));
    }
}
