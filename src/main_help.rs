//! 帮助页：每节一张卡片，左列键帽、右列动作；宽窗口分两列，绘制工具字母按网格排。
use super::{sidebar_width, widgets, SettingsView, Surface};
use crate::{
    capture_toolbar::Palette,
    preferences::Preferences,
    tray::shortcut_help::{help_sections, HelpGroup, HelpRow},
};
use gpui::{div, prelude::*, px, relative, rgb, AnyElement, Context, Div, FontWeight, Window};

/// 帮助正文最宽；比设置页宽，好让两列条目各自有足够的说明宽度。
const MAX_WIDTH: f32 = 960.;
/// 低于此正文宽度时退回单列。
const TWO_COLUMN_WIDTH: f32 = 700.;
/// 键帽列宽，容纳 `Ctrl+Shift+S` 这类三段组合。
const KEY_WIDTH: f32 = 168.;

impl SettingsView {
    pub(super) fn help_content(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let preferences = Preferences::load();
        let sections = help_sections(
            &preferences.hotkey_label(),
            &preferences.hotkey_label_for(crate::hotkey::HotkeyTarget::ClipboardPin),
        );
        // 正文左右各留 32（px_8）。
        let available = f32::from(window.viewport_size().width) - sidebar_width(window) - 64.;
        let columns = if available.min(MAX_WIDTH) >= TWO_COLUMN_WIDTH {
            2
        } else {
            1
        };
        let mut page = div()
            .w_full()
            .max_w(px(MAX_WIDTH))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_6();
        for section in sections {
            let groups = section
                .groups
                .iter()
                .map(|group| help_group(group, columns, palette, surface));
            let mut block =
                widgets::section(section.title, palette).child(widgets::card(surface, groups));
            if let Some(note) = section.note {
                block = block.child(widgets::note(note, palette));
            }
            page = page.child(block);
        }
        page
    }
}

fn help_group(group: &HelpGroup, columns: usize, palette: Palette, surface: Surface) -> AnyElement {
    let mut body = div().flex().flex_col().px_1().py_1();
    if let Some(title) = group.title {
        body = body.child(
            div()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(palette.muted))
                .child(title),
        );
    }
    if !group.rows.is_empty() {
        // 按列先填满再换列，保持从上到下的阅读顺序。
        let per_column = group.rows.len().div_ceil(columns);
        let chunks = group.rows.chunks(per_column);
        // 条目少于列数时补空列，让各组的键帽列对齐。
        let blanks = columns.saturating_sub(chunks.len());
        let mut grid = div().flex().gap_6();
        for chunk in chunks {
            grid = grid.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .children(chunk.iter().map(|row| help_row(row, palette, surface))),
            );
        }
        for _ in 0..blanks {
            grid = grid.child(div().flex_1());
        }
        body = body.child(grid);
    }
    if !group.tools.is_empty() {
        let width = relative(1. / (columns * 2) as f32);
        body = body.child(div().flex().flex_wrap().children(group.tools.iter().map(
            |(key, label)| {
                div()
                    .w(width)
                    .min_h(px(36.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(widgets::keycaps(&combo(key), palette, surface))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(palette.foreground))
                            .child(*label),
                    )
            },
        )));
    }
    body.into_any_element()
}

fn help_row(row: &HelpRow, palette: Palette, surface: Surface) -> Div {
    let mut keys = div()
        .w(px(KEY_WIDTH))
        .flex_shrink_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1();
    for (index, key) in row.keys.iter().enumerate() {
        if index > 0 {
            keys = keys.child(
                div()
                    .px_0p5()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child("/"),
            );
        }
        keys = keys.child(widgets::keycaps(&combo(key), palette, surface));
    }
    div()
        .min_h(px(36.))
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .gap_3()
        .child(keys)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(13.))
                .line_height(px(18.))
                .text_color(rgb(palette.foreground))
                .child(row.action),
        )
}

/// `Ctrl+Shift+S` 拆成逐键的键帽。
fn combo(key: &str) -> Vec<String> {
    key.split('+').map(str::to_owned).collect()
}
