//! 截图 OCR 右侧结果面板：行列表与图上文字互相定位，视图切换、复制、查找与敏感文字
//! 入口集中在面板内；选中文字的标记条贴在图上选区旁。面板、标记条与悬停色带只在界面
//! 绘制，不进入 Scene 或任何导出像素。
use super::{panel_click_host::PanelIntent, sensitive_host::SensitiveIntent, Operation};
use crate::{
    annotation::Point,
    capture_toolbar::Palette,
    capture_view::CaptureView,
    ocr_markup::TextMark,
    ocr_panel::{self, PanelLayout, PanelView, FOOTER_HEIGHT, HEADER_HEIGHT},
    pin_text_selection::SelectionText,
    pin_view::find_ui::FindAction,
    recognition::layout::{CopyLayoutMode, CopySpanKind, OcrCopyLayout},
    toolbar_icons::{self, Icon},
    toolbar_layout::PanelRect,
};
use gpui::{
    div, point, prelude::*, px, rgb, AnyElement, Context, FontWeight, MouseButton, ScrollHandle,
    SharedString, Window,
};
use std::{ops::Range, sync::Arc};

/// 与原位标记共用的四种标记色。
pub(super) const MARK_COLORS: [u32; 4] = toolbar_icons::TEXT_MARK_COLORS;
/// 识别层与悬停行在原图上的淡色带；选中仍用原有 0x609dff55。
pub(super) const LAYER_FILL: u32 = 0x609dff1a;
pub(super) const HOVER_FILL: u32 = 0x609dff40;
/// 表格页在原图上描出的单元格边线。
pub(super) const TABLE_BORDER: u32 = 0x609dffd0;
/// 单元格边线的画布像素宽度。
const TABLE_BORDER_WIDTH: f32 = 2.;

fn panel_tabs(table_requested: bool) -> Vec<(&'static str, &'static str, PanelView)> {
    [
        (
            "ocr-view-paragraphs",
            tr!("段落", "Paragraphs"),
            PanelView::Paragraphs,
        ),
        ("ocr-view-lines", tr!("逐行", "Lines"), PanelView::Lines),
        ("ocr-view-table", tr!("表格", "Table"), PanelView::Table),
        (
            "ocr-view-translation",
            tr!("译文", "Translation"),
            PanelView::Translation,
        ),
    ]
    .into_iter()
    .filter(|(_, _, view)| *view != PanelView::Table || table_requested)
    .collect()
}
const MARK_BUTTON: f32 = 30.;
const MARK_DOT: f32 = 22.;
const MARK_SEPARATOR: f32 = 9.;
const MARK_BAR_HEIGHT: f32 = 38.;
// 4 内边距×2 + 复制 + 翻译 + 两条分隔 + 4 标记 + 4 色点 + 11 处 2px 间隔 + 边框。
const MARK_BAR_WIDTH: f32 =
    8. + MARK_BUTTON * 2. + MARK_SEPARATOR * 2. + MARK_BUTTON * 4. + MARK_DOT * 4. + 22. + 2.;

/// 译文页的一张对照卡片：覆盖的识别行、原文与译文。
struct TranslationCard {
    rows: Range<usize>,
    source: String,
    translated: String,
}

/// 译文按空行分段后与段落视图的非空段落对齐成卡片。翻译服务可能合并或拆开段落，
/// 段数不一致时按累计字数比例对齐段界；流式中译文段数未超过原文时先逐段对应已收到的部分。
fn translation_cards(state: &PanelState, text: &str, streaming: bool) -> Vec<TranslationCard> {
    let sources: Vec<(&Range<usize>, &str)> = state
        .paragraphs
        .iter()
        .zip(&state.paragraph_text)
        .map(|(rows, source)| (rows, source.trim()))
        .filter(|(_, source)| !source.is_empty())
        .collect();
    let parts: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let groups = if streaming && parts.len() <= sources.len() {
        (0..parts.len()).map(|i| (i..i + 1, i..i + 1)).collect()
    } else {
        let weight = |text: &str| text.chars().count();
        ocr_panel::align_segments(
            &sources.iter().map(|(_, s)| weight(s)).collect::<Vec<_>>(),
            &parts.iter().map(|t| weight(t)).collect::<Vec<_>>(),
        )
    };
    groups
        .into_iter()
        .map(|(source, target)| TranslationCard {
            rows: sources[source.start].0.start..sources[source.end - 1].0.end,
            source: sources[source]
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>()
                .join("\n"),
            translated: parts[target].join("\n\n"),
        })
        .collect()
}

/// 面板的视图与悬停状态；行与段落划分在识别结果安装时一次算好。
pub(super) struct PanelState {
    view: PanelView,
    /// 当前悬停的识别行序号范围（段落视图为整段）。
    hover: Option<Range<usize>>,
    layer: bool,
    menu_open: bool,
    filter_open: bool,
    filter_scroll: ScrollHandle,
    lines: Vec<Range<usize>>,
    line_quads: Vec<Vec<[Point; 4]>>,
    paragraphs: Vec<Range<usize>>,
    paragraph_text: Vec<SharedString>,
    scroll: ScrollHandle,
    intent_epoch: Arc<()>,
}

#[derive(Clone, PartialEq)]
pub(super) struct PanelClickContext {
    view: PanelView,
    layer: bool,
    menu_open: bool,
    filter_open: bool,
    scroll_offset: gpui::Point<gpui::Pixels>,
    filter_scroll_offset: gpui::Point<gpui::Pixels>,
}

impl PanelState {
    /// `text` 须已平移到画布坐标，行色带与原位选字共用同一几何。
    pub(super) fn new(text: &SelectionText, layout: &OcrCopyLayout) -> Self {
        let lines = ocr_panel::line_ranges(&text.text);
        let line_quads = lines
            .iter()
            .map(|range| {
                if range.is_empty() {
                    Vec::new()
                } else {
                    text.highlights(range.clone())
                }
            })
            .collect();
        // LineJoin 的来源是被合并的换行分隔符；其末字节就是行尾的 '\n'。
        let mut joined: Vec<usize> = layout
            .copy_mapped(0..text.text.len(), CopyLayoutMode::Auto)
            .map(|copied| {
                copied
                    .spans
                    .iter()
                    .filter(|span| span.kind == CopySpanKind::LineJoin)
                    .filter(|span| !span.source_range.is_empty())
                    .map(|span| span.source_range.end - 1)
                    .collect()
            })
            .unwrap_or_default();
        joined.sort_unstable();
        let paragraphs = ocr_panel::paragraph_groups(&lines, &joined);
        let paragraph_text = paragraphs
            .iter()
            .map(|rows| {
                let span = ocr_panel::byte_span(&lines, rows.clone()).unwrap_or(0..0);
                layout
                    .copy(span.clone(), CopyLayoutMode::Auto)
                    .unwrap_or_else(|_| text.text[span].to_owned())
                    .into()
            })
            .collect();
        Self {
            view: PanelView::default(),
            hover: None,
            layer: true,
            menu_open: false,
            filter_open: false,
            filter_scroll: ScrollHandle::new(),
            lines,
            line_quads,
            paragraphs,
            paragraph_text,
            scroll: ScrollHandle::new(),
            intent_epoch: Arc::new(()),
        }
    }

    fn paragraph_of(&self, line: usize) -> Option<usize> {
        self.paragraphs.iter().position(|rows| rows.contains(&line))
    }

    /// 图上悬停一行时联动的行范围：段落视图联动整段。
    fn hover_rows(&self, line: usize) -> Range<usize> {
        match self.view {
            PanelView::Paragraphs | PanelView::Translation => self
                .paragraph_of(line)
                .map_or(line..line + 1, |p| self.paragraphs[p].clone()),
            _ => line..line + 1,
        }
    }

    pub(super) fn layer_quads(
        &self,
        filter: &super::filter_host::FilterState,
        text: &SelectionText,
    ) -> Vec<[Point; 4]> {
        if !self.layer {
            return Vec::new();
        }
        filter.quads(text, false)
    }

    pub(super) fn hover_quads(&self) -> Vec<[Point; 4]> {
        self.hover.clone().map_or_else(Vec::new, |rows| {
            rows.filter_map(|row| self.line_quads.get(row))
                .flatten()
                .copied()
                .collect()
        })
    }

    pub(super) fn close_menu(&mut self) {
        self.set_menu(false);
        self.set_filter_open(false);
    }

    pub(super) fn view(&self) -> PanelView {
        self.view
    }

    pub(super) fn menu_open(&self) -> bool {
        self.menu_open
    }

    pub(super) fn filter_open(&self) -> bool {
        self.filter_open
    }

    pub(super) fn filter_scroll(&self) -> &ScrollHandle {
        &self.filter_scroll
    }

    /// 图片翻译按段落视图的划分取块；行四角已在画布坐标。
    pub(super) fn translation_blocks(&self) -> Vec<crate::image_translate::SourceBlock> {
        crate::image_translate::blocks_from_paragraphs(
            &self.paragraphs,
            &self.paragraph_text,
            &self.line_quads,
        )
    }

    pub(super) fn set_menu(&mut self, open: bool) {
        if open {
            self.set_filter_open(false);
        }
        if self.menu_open != open {
            self.menu_open = open;
            self.intent_epoch = Arc::new(());
        }
    }

    pub(super) fn set_filter_open(&mut self, open: bool) {
        if open {
            self.set_menu(false);
        }
        if self.filter_open != open {
            self.filter_open = open;
            if open {
                self.filter_scroll.set_offset(point(px(0.), px(0.)));
            }
            self.intent_epoch = Arc::new(());
        }
    }

    pub(super) fn set_layer(&mut self, visible: bool) {
        if self.layer != visible {
            self.layer = visible;
            self.intent_epoch = Arc::new(());
        }
    }

    pub(super) fn set_view(&mut self, view: PanelView) {
        if self.view != view {
            self.view = view;
            self.hover = None;
            self.scroll.set_offset(point(px(0.), px(0.)));
            self.intent_epoch = Arc::new(());
        }
        self.close_menu();
    }

    pub(super) fn click_state(&self) -> (Arc<()>, PanelClickContext) {
        (
            Arc::clone(&self.intent_epoch),
            PanelClickContext {
                view: self.view,
                layer: self.layer,
                menu_open: self.menu_open,
                filter_open: self.filter_open,
                scroll_offset: self.scroll.offset(),
                filter_scroll_offset: self.filter_scroll.offset(),
            },
        )
    }
}

fn hint(text: &'static str) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView {
    move |_, cx| cx.new(move |_| crate::pin_view::TextToolHint(text)).into()
}

fn icon_button(
    id: &'static str,
    icon: Icon,
    tip: &'static str,
    active: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(28.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .when(active, |b| b.bg(rgb(p.selected)))
        .hover(move |s| s.bg(rgb(if active { p.selected } else { p.hover })))
        .child(
            toolbar_icons::action(icon, if active { p.accent } else { p.foreground }).size(px(16.)),
        )
        .tooltip(hint(tip))
}

fn panel_shell(id: &'static str, layout: PanelLayout, p: Palette) -> gpui::Stateful<gpui::Div> {
    let r = layout.panel;
    div()
        .id(id)
        .absolute()
        .left(px(r.x))
        .top(px(r.y))
        .w(px(r.width))
        .h(px(r.height))
        .flex()
        .flex_col()
        .rounded_lg()
        .bg(rgb(p.background))
        .border_1()
        .border_color(rgb(p.border))
        .shadow_lg()
        .text_color(rgb(p.foreground))
        .text_size(px(13.))
        .overflow_hidden()
        .cursor(gpui::CursorStyle::Arrow)
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}

fn panel_header(meta: SharedString, p: Palette) -> gpui::Div {
    div()
        .h(px(HEADER_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(2.))
        .pl(px(14.))
        .pr(px(8.))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(8.))
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_nowrap()
                        .child(tr!("文字识别", "Text")),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .truncate()
                        .child(meta),
                ),
        )
}

const STRIP_PAD: f32 = 6.;
const STRIP_GAP: f32 = 2.;
const TAB_PAD: f32 = 4.;
const TAB_TEXT: f32 = 13.;
/// 表格／译文页签可能出现的小圆点（4px 间距 + 5px 点）。
const TAB_BADGE: f32 = 9.;
const CHIP_TEXT: f32 = 12.;
const CHIP_MARGIN: f32 = 4.;
const CHIP_ICON_ONLY: f32 = 28.;
/// 带文字的“对照”：左 6 + 图标 14 + 间距 5 + 文字 + 右 8。
const CHIP_CHROME: f32 = 6. + 14. + 5. + 8.;

/// 页签行如何容纳页签与行尾“对照”按钮；按实测文字宽度决定，不按界面语言写死。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StripFit {
    /// 页签完整显示，“对照”带文字。
    Full,
    /// “对照”只留图标。
    IconChip,
    /// 仍放不下：页签允许缩窄并以省略号截断。
    Squeeze,
}

/// `available` 为页签行可用宽度，`tabs` 为各页签加粗文字（含可能的小圆点）宽度。
fn strip_fit(available: f32, tabs: &[f32], chip_label: f32) -> StripFit {
    let tabs_width: f32 = tabs.iter().map(|w| w + TAB_PAD * 2.).sum::<f32>()
        + STRIP_PAD * 2.
        + STRIP_GAP * tabs.len() as f32
        + CHIP_MARGIN;
    if tabs_width + CHIP_CHROME + chip_label <= available {
        StripFit::Full
    } else if tabs_width + CHIP_ICON_ONLY <= available {
        StripFit::IconChip
    } else {
        StripFit::Squeeze
    }
}

/// 用当前窗口字体测量单行文字宽度（向上取整留出亚像素余量）。
fn text_width(window: &Window, text: &'static str, size: f32, weight: FontWeight) -> f32 {
    let font = gpui::Font {
        weight,
        ..window.text_style().font()
    };
    let line = window.text_system().shape_line(
        text.into(),
        px(size),
        &[gpui::TextRun {
            len: text.len(),
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    f32::from(line.width).ceil() + 1.
}

/// 下划线页签行：页签平分整行，整格可点；行尾可放当前页的小开关。
fn views_strip(p: Palette) -> gpui::Div {
    div()
        .mb(px(6.))
        .h(px(40.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(STRIP_GAP))
        .px(px(STRIP_PAD))
        .border_b_1()
        .border_color(rgb(p.border))
}

fn view_tab(
    id: &'static str,
    label: &'static str,
    active: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .group(id)
        .relative()
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .px(px(TAB_PAD))
        .text_size(px(TAB_TEXT))
        .whitespace_nowrap()
        .text_color(rgb(if active { p.foreground } else { p.muted }))
        // 悬停底色比整格略矮，留出下划线位置；先画在文字下面。
        .child(
            div()
                .absolute()
                .top(px(5.))
                .bottom(px(5.))
                .left_0()
                .right_0()
                .rounded_md()
                .group_hover(id, move |s| s.bg(rgb(p.hover))),
        )
        // 始终按加粗宽度占位，再把实际文字居中叠上，切页时页签宽度不跳；
        // 页签被压窄（StripFit::Squeeze）时文字以省略号截断。
        .child(
            div()
                .relative()
                .min_w(px(0.))
                .overflow_hidden()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .opacity(0.)
                        .truncate()
                        .child(label),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .when(active, |l| l.font_weight(FontWeight::SEMIBOLD))
                                .child(label),
                        ),
                ),
        )
        .when(active, |t| {
            t.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(-1.))
                    .flex()
                    .justify_center()
                    .child(div().w(px(28.)).h(px(2.)).rounded_full().bg(rgb(p.accent))),
            )
        })
}

/// 译文页“显示原文”开关：本进程首次用到时读偏好，切换后立即写回 settings.ini。
fn translation_show_source() -> &'static std::sync::atomic::AtomicBool {
    static SHOW: std::sync::OnceLock<std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();
    SHOW.get_or_init(|| {
        std::sync::atomic::AtomicBool::new(
            crate::preferences::Preferences::load().translation_show_source,
        )
    })
}

impl CaptureView {
    pub(super) fn set_translation_show_source(&mut self, show: bool, cx: &mut Context<Self>) {
        translation_show_source().store(show, std::sync::atomic::Ordering::Relaxed);
        let mut preferences = crate::preferences::Preferences::load();
        if preferences.translation_show_source != show {
            preferences.translation_show_source = show;
            if preferences.save().is_err() {
                self.status = tr!(
                    "“显示原文”未能保存，下次截图会恢复原设置",
                    "Couldn't save “Show source”; it resets next capture"
                )
                .into();
            }
        }
        cx.notify();
    }
}

pub(super) fn mini_switch(on: bool, p: Palette) -> gpui::Div {
    div()
        .relative()
        .w(px(28.))
        .h(px(16.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(if on { p.accent } else { p.border }))
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(if on { 14. } else { 2. }))
                .size(px(12.))
                .rounded_full()
                .bg(rgb(p.background)),
        )
}

fn primary_button(id: &'static str, p: Palette) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(32.))
        .px(px(14.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded_md()
        .bg(rgb(p.accent))
        .text_color(rgb(p.background))
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
}

fn skeleton(p: Palette) -> gpui::Div {
    let mut bars = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(16.))
        .pt(px(6.));
    for width in [0.92, 0.78, 0.84, 0.6, 0.7, 0.88, 0.52, 0.66, 0.74, 0.4] {
        bars = bars.child(
            div()
                .h(px(10.))
                .w(gpui::relative(width))
                .rounded_full()
                .bg(rgb(p.hover)),
        );
    }
    bars
}

/// 单元格矩形（识别图片局部坐标）平移到画布后的四条边线与填充四角。
fn cell_quads(rect: crate::geometry::Rect, origin: (f32, f32)) -> ([[Point; 4]; 4], [Point; 4]) {
    let x0 = origin.0 + rect.x as f32;
    let y0 = origin.1 + rect.y as f32;
    let x1 = x0 + rect.width as f32;
    let y1 = y0 + rect.height as f32;
    let w = TABLE_BORDER_WIDTH / 2.;
    let quad = |l: f32, t: f32, r: f32, b: f32| {
        [
            Point { x: l, y: t },
            Point { x: r, y: t },
            Point { x: r, y: b },
            Point { x: l, y: b },
        ]
    };
    (
        [
            quad(x0 - w, y0 - w, x1 + w, y0 + w),
            quad(x0 - w, y1 - w, x1 + w, y1 + w),
            quad(x0 - w, y0 + w, x0 + w, y1 - w),
            quad(x1 - w, y0 + w, x1 + w, y1 - w),
        ],
        quad(x0, y0, x1, y1),
    )
}

impl CaptureView {
    /// 表格页的图上可视化：全部单元格边线与悬停单元格；没有表格时为 None。
    pub(super) fn table_overlay_quads(
        &self,
        selected: crate::geometry::Rect,
    ) -> Option<(Vec<[Point; 4]>, Vec<[Point; 4]>)> {
        let grid = self.cached_table_grid()?;
        let origin = (selected.x as f32, selected.y as f32);
        let mut borders = Vec::with_capacity(grid.cells.len() * 4);
        let mut hover = Vec::new();
        for (index, cell) in grid.cells.iter().enumerate() {
            let (edges, fill) = cell_quads(cell.rect, origin);
            borders.extend(edges);
            if self.table_hover == Some(index) {
                hover.push(fill);
            }
        }
        Some((borders, hover))
    }

    /// 识别行是否属于当前表格：正文中的表格只含其来源行，独立表格含全部行。
    pub(super) fn table_line_set(&self, count: usize) -> Vec<bool> {
        match self.cached_table_region() {
            Some(lines) => {
                let mut set = vec![false; count];
                for line in lines {
                    if let Some(slot) = set.get_mut(*line) {
                        *slot = true;
                    }
                }
                set
            }
            None => vec![true; count],
        }
    }

    /// 段落视图“复制全文”：正文按段落，表格部分按制表符分隔放在原位置；没有表格时为 None。
    pub(super) fn ocr_mixed_text(&self) -> Option<String> {
        let overlay = self.ocr_overlay.as_ref()?;
        let state = &overlay.panel;
        let tsv = crate::table_export::tsv(self.cached_table_grid()?)?;
        let lines = self.table_line_set(state.lines.len());
        let mut parts: Vec<String> = Vec::new();
        let mut placed = false;
        for (rows, text) in state.paragraphs.iter().zip(&state.paragraph_text) {
            let inside = rows.clone().filter(|row| lines[*row]).count();
            if inside > 0 && !placed {
                placed = true;
                parts.push(tsv.clone());
            }
            if inside < rows.len() && !text.trim().is_empty() {
                parts.push(text.to_string());
            }
        }
        Some(parts.join("\n"))
    }

    /// 表格网格：列宽按原图列宽比例，悬停单元格与原图对应格联动加深。
    fn render_table_grid(
        &self,
        grid: &crate::table_grid::TableGrid,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let row_height = 30.;
        let left = grid.column_edges[0] as f32;
        let total = (*grid.column_edges.last().unwrap_or(&0) as f32 - left).max(1.);
        let x = |column: usize| (grid.column_edges[column] as f32 - left) / total;
        let mut table = div()
            .mx(px(4.))
            .mt(px(2.))
            .flex_shrink_0()
            .relative()
            .h(px(row_height * grid.rows() as f32))
            .border_1()
            .border_color(rgb(p.border))
            .rounded_md()
            .overflow_hidden();
        for (index, cell) in grid.cells.iter().enumerate() {
            let hot = self.table_hover == Some(index);
            let start = x(cell.column);
            let end = x(cell.column + cell.colspan);
            table = table.child(
                div()
                    .id(("ocr-table-cell", index))
                    .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
                        let next = if *hovered {
                            Some(index)
                        } else if view.table_hover == Some(index) {
                            None
                        } else {
                            return;
                        };
                        if view.table_hover != next {
                            view.table_hover = next;
                            cx.notify();
                        }
                    }))
                    .absolute()
                    .left(gpui::relative(start))
                    .top(px(cell.row as f32 * row_height))
                    .w(gpui::relative(end - start))
                    .h(px(cell.rowspan as f32 * row_height))
                    .border_1()
                    .border_color(rgb(p.border))
                    .when(cell.row == 0, |l| {
                        l.bg(rgb(p.hover)).font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(hot, |l| l.bg(rgb(p.selected)))
                    .px(px(6.))
                    .py(px(5.))
                    .text_size(px(12.5))
                    .truncate()
                    .child(cell.text.clone()),
            );
        }
        table
    }

    /// 窗口 UI 使用固定 DIP；图片编辑避开顶部图片控件，截图保留显示器工作区。
    pub(crate) fn ocr_ui_work(&self, window: &Window) -> Option<PanelRect> {
        if self.pin_mode {
            return None;
        }
        let projection = self.ocr_projection(window)?;
        if self.image_canvas.is_some() {
            return Some(projection.client_work(92., 8.));
        }
        let scale = window.scale_factor();
        let monitor = self.ui_monitor(self.toolbar_monitor);
        Some(PanelRect {
            x: monitor.x as f32 / scale,
            y: monitor.y as f32 / scale,
            width: monitor.width as f32 / scale,
            height: monitor.height as f32 / scale,
        })
    }

    /// 原图选区先投到客户区 DIP；面板自身不随图像缩放、平移或预览 ROI 改变尺寸。
    pub(crate) fn ocr_panel_frame(&self, window: &Window) -> Option<PanelLayout> {
        let selected = self.selected().ok()?;
        let projection = self.ocr_projection(window)?;
        let toolbar = self.toolbar_layout(selected, window.scale_factor()).primary;
        Some(ocr_panel::panel_layout(
            self.ocr_ui_work(window)?,
            projection.rect(selected),
            Some(toolbar),
        ))
    }

    /// 选区整段识别进行中（行级重识别另有自己的进度）。
    pub(crate) fn ocr_panel_busy(&self) -> bool {
        self.ocr_overlay.is_none()
            && self.ocr_progress_replaces_status()
            && self.local_ocr_progress().is_none()
    }

    /// 识别进行中的骨架面板，与完成后的面板同位置，避免结果出现时跳动。
    pub(crate) fn render_ocr_busy_panel(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let layout = self.ocr_panel_frame(window)?;
        let p = crate::theme::palette(cx);
        let mut views = views_strip(p).opacity(0.5);
        for (id, label) in [
            ("ocr-busy-lines", tr!("原文", "Lines")),
            ("ocr-busy-paragraphs", tr!("段落", "Paragraphs")),
            ("ocr-busy-table", tr!("表格", "Table")),
        ] {
            if id == "ocr-busy-table"
                && !self.table_recognition_pending()
                && self.quick_action.current() != Some(crate::quick_action::Step::TableCopy)
            {
                continue;
            }
            views = views.child(view_tab(id, label, id == "ocr-busy-lines", p));
        }
        Some(
            panel_shell("ocr-panel-busy", layout, p)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(panel_header(tr!("识别中…", "Recognizing…").into(), p))
                .child(views)
                .child(skeleton(p))
                .child(
                    div()
                        .h(px(FOOTER_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_end()
                        .px(px(12.))
                        .border_t_1()
                        .border_color(rgb(p.border))
                        .child(
                            primary_button("ocr-busy-copy", p)
                                .opacity(0.45)
                                .child(tr!("复制全文", "Copy all")),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn ocr_panel_set_hover(
        &mut self,
        rows: Option<Range<usize>>,
        scroll: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay) = self.ocr_overlay.as_mut() else {
            return;
        };
        if overlay.panel.hover == rows {
            return;
        }
        if scroll {
            overlay.panel_click.borrow_mut().cancel();
        }
        let item = scroll
            .then(|| {
                rows.as_ref()
                    .and_then(|r| self.ocr_panel_item_index(r.start))
            })
            .flatten();
        let Some(overlay) = self.ocr_overlay.as_mut() else {
            return;
        };
        let panel = &mut overlay.panel;
        if let Some(item) = item {
            panel.scroll.scroll_to_item(item);
        }
        panel.hover = rows;
        cx.notify();
    }

    /// 识别行在当前页签列表中的子元素序号，用于图上悬停时滚到可见；须与各页签的渲染顺序一致。
    fn ocr_panel_item_index(&self, line: usize) -> Option<usize> {
        let overlay = self.ocr_overlay.as_ref()?;
        let state = &overlay.panel;
        let paragraph = state.paragraph_of(line);
        match state.view {
            PanelView::Lines => (0..state.lines.len())
                .filter(|i| !overlay.filter.model.is_hidden(*i))
                .position(|i| i == line),
            PanelView::Paragraphs => {
                let paragraph = paragraph?;
                let Some(lines) = self
                    .cached_table_grid()
                    .map(|_| self.table_line_set(state.lines.len()))
                else {
                    return Some(paragraph);
                };
                // 混排：首个含表格行的段落位置插入一张表格，整段都在表格里的段落不单列。
                let mut item = 0;
                let mut table_item = None;
                for (index, rows) in state.paragraphs.iter().enumerate() {
                    let inside = rows.clone().filter(|row| lines[*row]).count();
                    if inside > 0 && table_item.is_none() {
                        table_item = Some(item);
                        if index == paragraph {
                            return table_item;
                        }
                        item += 1;
                    }
                    if inside == rows.len() {
                        if index == paragraph {
                            return table_item;
                        }
                        continue;
                    }
                    if index == paragraph {
                        return Some(item);
                    }
                    item += 1;
                }
                None
            }
            PanelView::Translation => self
                .ocr_panel_translation_cards()?
                .iter()
                .position(|card| card.rows.contains(&line)),
            PanelView::Table => None,
        }
    }

    /// 译文页逐段对照的卡片；未开逐段、翻译失败或尚无译文时为 None（整块显示）。
    fn ocr_panel_translation_cards(&self) -> Option<Vec<TranslationCard>> {
        use crate::translate::TranslationState;
        if !self.translation_by_paragraph {
            return None;
        }
        let state = &self.ocr_overlay.as_ref()?.panel;
        let translation = self.current_translation()?;
        let (text, streaming) = match &translation.state {
            TranslationState::Pending => (translation.partial(), true),
            TranslationState::Done(text) => (text.as_str(), false),
            TranslationState::Failed(_) => return None,
        };
        let cards = translation_cards(state, text, streaming);
        (!cards.is_empty()).then_some(cards)
    }

    /// 图上指针悬停：命中识别行时加亮面板对应行并滚到可见；指针在面板上时不处理。
    pub(super) fn ocr_panel_pointer_moved(
        &mut self,
        p: Point,
        position: gpui::Point<gpui::Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .ocr_panel_frame(window)
            .is_some_and(|layout| layout.contains(f32::from(position.x), f32::from(position.y)))
        {
            return;
        }
        let Some(overlay) = self.ocr_overlay.as_ref() else {
            return;
        };
        let rows = ocr_panel::line_at(&overlay.panel.line_quads, p)
            .map(|line| overlay.panel.hover_rows(line));
        self.ocr_panel_set_hover(rows, true, cx);
    }

    /// 面板点选行：Shift 与已有选择合并，再点唯一选中的同一范围则取消。
    pub(super) fn ocr_panel_select_rows(
        &mut self,
        rows: Range<usize>,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.validate_ocr_overlay();
        if !self.ocr_overlay_ready() || self.ocr_overlay_dragging() {
            return;
        }
        self.cancel_ocr_sensitive_preview();
        self.clear_find_preview();
        if window.is_window_active() && !self.focus.is_focused(window) {
            window.focus(&self.focus);
        }
        let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
        overlay.panel.close_menu();
        let lines = &overlay.panel.lines;
        let current = overlay.selected.clone().filter(|range| !range.is_empty());
        let rows = match current.as_ref().filter(|_| extend).and_then(|selected| {
            Some((
                ocr_panel::line_of(lines, selected.start)?,
                ocr_panel::line_of(lines, selected.end - 1)?,
            ))
        }) {
            Some((first, last)) => first.min(rows.start)..(last + 1).max(rows.end),
            None => rows,
        };
        let Some(span) = ocr_panel::byte_span(lines, rows) else {
            return;
        };
        if span.is_empty() || !extend && current.as_ref() == Some(&span) {
            overlay.clear_selection();
        } else {
            overlay.show_selection = true;
            overlay.select_range(span);
        }
        cx.notify();
    }

    /// 双击面板行：复制该行（段落视图为该段，折行合并）后结束截图。
    /// 首次按下可能已选中同一范围，第二次按下不再走切换，避免把选择清掉。
    pub(super) fn ocr_panel_copy_rows_and_close(
        &mut self,
        rows: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((selected, view)) = self.ocr_overlay.as_ref().map(|overlay| {
            let span = ocr_panel::byte_span(&overlay.panel.lines, rows.clone());
            (
                span.is_some() && overlay.selected == span,
                overlay.panel.view,
            )
        }) else {
            return;
        };
        if !selected {
            self.ocr_panel_select_rows(rows, false, window, cx);
        }
        let operation = if view == PanelView::Paragraphs {
            Operation::CopyParagraphs
        } else {
            Operation::CopyText
        };
        self.ocr_overlay_copy_and_close(operation, window, cx);
    }

    pub(crate) fn set_ocr_panel_view(&mut self, view: PanelView, cx: &mut Context<Self>) {
        if view == PanelView::Table && !self.table_forced() {
            return;
        }
        self.cancel_ocr_panel_click();
        self.table_hover = None;
        if let Some(overlay) = self.ocr_overlay.as_mut() {
            overlay.panel.set_view(view);
            cx.notify();
        }
        // 译文页是翻译的触发方式：进入时没有当前译文就翻译全文（按段落）。
        if view == PanelView::Translation && self.current_translation().is_none() {
            self.translate_ocr_full_text(cx);
        }
    }

    /// 原位结果仍对应当前截图范围与内容。
    pub(crate) fn ocr_overlay_current(&self) -> bool {
        self.ocr_overlay
            .as_ref()
            .is_some_and(|overlay| self.current_ocr_target().as_ref() == Some(&overlay.target))
    }

    /// 全文按段落送去翻译，段间空行让译文能逐段对照。
    pub(crate) fn translate_ocr_full_text(&mut self, cx: &mut Context<Self>) {
        // 独立表格只翻译锚定单元格；几何和合并关系由本机持有。
        if self.cached_table_grid().is_some() && self.cached_table_region().is_none() {
            self.translate_ocr_table(cx);
            return;
        }
        if let Some(text) = self.ocr_mixed_text() {
            // 正文混排表格的 TSV 由协议层逐格保护，不把它重组为段落。
            self.start_translation(text, cx);
            return;
        }
        let Some(text) = self.ocr_overlay.as_ref().map(|overlay| {
            overlay
                .panel
                .paragraph_text
                .iter()
                .map(|paragraph| paragraph.trim())
                .filter(|paragraph| !paragraph.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        }) else {
            return;
        };
        self.start_translation(text, cx);
        self.translation_by_paragraph = self.translation_pending();
    }

    pub(super) fn render_ocr_panel(
        &self,
        layout: PanelLayout,
        find_bar: Option<AnyElement>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let overlay = self.ocr_overlay.as_ref()?;
        let state = &overlay.panel;
        let p = crate::theme::palette(cx);
        let text = &overlay.text.text;
        let selection = overlay.selected.clone().filter(|range| !range.is_empty());
        let line_count = state
            .lines
            .iter()
            .filter(|range| !text[(*range).clone()].trim().is_empty())
            .count();
        let find_open = self.ocr_find.is_some();
        let header = panel_header(trf!("{} 行", "{} lines", line_count).into(), p)
            .child(self.bind_ocr_panel_click(
                icon_button(
                    "ocr-panel-find",
                    Icon::Search,
                    tr!("查找 · Ctrl+F", "Find · Ctrl+F"),
                    find_open,
                    p,
                ),
                "ocr-panel-find",
                PanelIntent::Find(if find_open {
                    FindAction::Close
                } else {
                    FindAction::Open
                }),
                window,
                cx,
            ))
            .child(self.bind_ocr_panel_click(
                icon_button(
                    "ocr-panel-edit",
                    Icon::Pen,
                    tr!("编辑全文", "Edit full text"),
                    false,
                    p,
                ),
                "ocr-panel-edit",
                PanelIntent::Operation(Operation::FullText),
                window,
                cx,
            ))
            .child(self.bind_ocr_panel_click(
                icon_button(
                    "ocr-panel-more",
                    Icon::More,
                    tr!("更多", "More"),
                    state.menu_open,
                    p,
                ),
                "ocr-panel-more",
                PanelIntent::Menu(!state.menu_open),
                window,
                cx,
            ))
            .child(self.bind_ocr_panel_click(
                icon_button(
                    "ocr-panel-close",
                    Icon::Close,
                    tr!("关闭 · Esc", "Close · Esc"),
                    false,
                    p,
                ),
                "ocr-panel-close",
                PanelIntent::Operation(Operation::Back),
                window,
                cx,
            ));

        let tabs = panel_tabs(self.table_forced());
        // 按实测文字宽度排布，任何界面语言都不溢出。
        let fit = strip_fit(
            layout.panel.width - 2.,
            &tabs
                .iter()
                .map(|(_, label, view)| {
                    text_width(window, label, TAB_TEXT, FontWeight::SEMIBOLD)
                        + if matches!(view, PanelView::Table | PanelView::Translation) {
                            TAB_BADGE
                        } else {
                            0.
                        }
                })
                .collect::<Vec<_>>(),
            text_width(window, tr!("对照", "Source"), CHIP_TEXT, FontWeight::NORMAL),
        );
        let mut views = views_strip(p);
        for (id, label, view) in tabs {
            let active = state.view == view;
            // 主动表格结果或已有译文在未选中的页签上提示。
            let badge = !active
                && match view {
                    PanelView::Table => self.cached_table_grid().is_some(),
                    PanelView::Translation => self.current_translation().is_some(),
                    _ => false,
                };
            views = views.child(
                self.bind_ocr_panel_click(
                    view_tab(id, label, active, p)
                        .when(fit == StripFit::Squeeze, |t| {
                            t.min_w(px(0.)).overflow_hidden()
                        })
                        .when(badge, |t| {
                            t.gap(px(4.))
                                .child(div().size(px(5.)).rounded_full().bg(rgb(p.accent)))
                        })
                        .when(!active, |t| {
                            t.cursor_pointer()
                                .hover(move |s| s.text_color(rgb(p.foreground)))
                        }),
                    id,
                    PanelIntent::View(view),
                    window,
                    cx,
                ),
            );
        }
        // “对照”只在译文页可用；其它页透明占位，切页时页签宽度不变、不在鼠标下跳动。
        let on_translation = state.view == PanelView::Translation;
        let show_source = translation_show_source().load(std::sync::atomic::Ordering::Relaxed);
        let source_color = if show_source { p.accent } else { p.muted };
        let compact = fit != StripFit::Full;
        let source_chip = div()
            .id("ocr-translation-show-source")
            .flex_shrink_0()
            .ml(px(CHIP_MARGIN))
            .h(px(24.))
            .when(compact, |c| c.w(px(CHIP_ICON_ONLY)))
            .when(!compact, |c| c.pl(px(6.)).pr(px(8.)))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(5.))
            .rounded_md()
            .text_size(px(CHIP_TEXT))
            .whitespace_nowrap()
            .text_color(rgb(source_color))
            .when(show_source, |c| c.bg(rgb(p.selected)))
            .child(toolbar_icons::action(Icon::Translate, source_color).size(px(14.)))
            .when(!compact, |c| c.child(tr!("对照", "Source")));
        views = views.child(if on_translation {
            self.bind_ocr_panel_click(
                source_chip
                    .cursor_pointer()
                    .when(!show_source, |c| c.hover(move |s| s.bg(rgb(p.hover))))
                    .tooltip(hint(tr!(
                        "在每段译文上方显示原文；设置会记住",
                        "Show source above each paragraph; remembered"
                    ))),
                "ocr-translation-show-source",
                PanelIntent::ShowSource(!show_source),
                window,
                cx,
            )
        } else {
            source_chip.opacity(0.)
        });

        let body = self.render_ocr_panel_body(selection.as_ref(), window, p, cx);

        let mut panel = panel_shell("ocr-panel", layout, p)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.cancel_ocr_panel_click();
                    if let Some(overlay) = view.ocr_overlay.as_mut() {
                        if overlay.panel.menu_open || overlay.panel.filter_open {
                            overlay.panel.close_menu();
                            cx.notify();
                        }
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                if let Some(overlay) = &mut view.ocr_overlay {
                    if overlay.panel.menu_open || overlay.panel.filter_open {
                        overlay.panel.close_menu();
                        view.cancel_ocr_panel_click();
                        cx.notify();
                    }
                }
            }))
            .child(header);
        let tools = self.render_ocr_panel_mark_controls(layout, window, cx);
        // 短面板仍保留标题与原底栏；回退工具和tabs在中段滚动，不能挤掉旧入口。
        let mut middle = div()
            .id("ocr-panel-middle")
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_y_scroll();
        if let Some(tools) = tools {
            middle = middle.child(tools);
        }
        middle = middle.child(views);
        if let Some(bar) = find_bar {
            middle = middle.child(div().px(px(10.)).pb(px(6.)).flex_shrink_0().child(bar));
        }
        middle = middle.child(body);
        panel = panel.child(middle).child(self.render_ocr_panel_footer(
            selection.as_ref(),
            window,
            p,
            cx,
        ));
        if state.menu_open {
            panel = panel.child(self.render_ocr_panel_menu(window, p, cx));
        }
        if state.filter_open {
            panel = panel.child(self.render_ocr_filter_popup(layout, window, p, cx));
        }
        Some(panel.into_any_element())
    }

    fn render_ocr_panel_body(
        &self,
        selection: Option<&Range<usize>>,
        window: &Window,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let overlay = self
            .ocr_overlay
            .as_ref()
            .expect("面板仅在原位文字层存在时渲染");
        let state = &overlay.panel;
        let text = &overlay.text.text;
        let mut body = div()
            .id("ocr-panel-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&state.scroll)
            .flex()
            .flex_col()
            .gap(px(1.))
            .px(px(6.))
            .pb(px(8.));
        if text.trim().is_empty() {
            return body.child(
                div()
                    .p(px(10.))
                    .text_color(rgb(p.muted))
                    .child(tr!("未识别到文字", "No text recognized")),
            );
        }
        let hover = state.hover.clone();
        let hovered = |rows: &Range<usize>| {
            hover
                .as_ref()
                .is_some_and(|h| h.start < rows.end && rows.start < h.end)
        };
        let row_base = |id: (&'static str, usize), selected: bool, hot: bool| {
            div()
                .id(id)
                .flex_shrink_0()
                .rounded_md()
                .cursor(gpui::CursorStyle::PointingHand)
                .when(selected, |r| r.bg(rgb(p.selected)))
                .when(hot && !selected, |r| r.bg(rgb(p.hover)))
        };
        match state.view {
            PanelView::Lines => {
                // 重识别只对唯一选中的可信单行开放，按钮放在该行行尾。
                let retry = self.local_ocr_line_click_stamp();
                let retry_line = retry.as_ref().and(selection).and_then(|selected| {
                    let first = ocr_panel::line_of(&state.lines, selected.start)?;
                    (ocr_panel::line_of(&state.lines, selected.end - 1)? == first).then_some(first)
                });
                for (index, range) in state.lines.iter().enumerate() {
                    if overlay.filter.model.is_hidden(index) {
                        continue;
                    }
                    let rows = index..index + 1;
                    let selected = overlay.filter.model.selected.contains(&index)
                        || selection.is_some_and(|s| ocr_panel::row_selected(range, s));
                    let content = text[range.clone()].trim_end_matches('\r').to_owned();
                    let blank = content.trim().is_empty();
                    let mut row = row_base(("ocr-panel-line", index), selected, hovered(&rows))
                        .flex()
                        .items_start()
                        .gap(px(4.))
                        .py(px(3.))
                        .pr(px(4.))
                        .child(
                            div()
                                .w(px(26.))
                                .flex_shrink_0()
                                .text_size(px(11.))
                                .line_height(px(20.))
                                .text_right()
                                .text_color(rgb(if selected { p.accent } else { p.muted }))
                                .child((index + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_size(px(13.))
                                .line_height(px(20.))
                                .when(blank, |t| t.text_color(rgb(p.muted)))
                                .child(if blank { " ".to_owned() } else { content }),
                        );
                    if retry_line == Some(index) {
                        let stamp = retry.clone();
                        let down = retry.clone();
                        row = row.child(
                            div()
                                .id("ocr-retry-line")
                                .size(px(22.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(move |s| s.bg(rgb(p.background)))
                                .child(
                                    toolbar_icons::action(Icon::Reselect, p.accent).size(px(14.)),
                                )
                                .tooltip(hint(tr!(
                                    "重识别此行（结果另行显示，不替换原文）",
                                    "Recognize this line again (shown separately)"
                                )))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, event, window, cx| {
                                        if let Some(stamp) = &down {
                                            view.arm_local_retry_click(stamp, event, window, cx);
                                        }
                                        cx.stop_propagation();
                                    }),
                                )
                                .on_click(cx.listener(move |view, event, window, cx| {
                                    if let Some(stamp) = &stamp {
                                        view.queue_local_retry_click(stamp, event, window, cx);
                                    }
                                })),
                        );
                    }
                    body = body.child(self.bind_ocr_panel_row(row, rows, window, cx));
                }
            }
            PanelView::Paragraphs => {
                // 段落与表格混排：表格所在的段落整体换成一张内嵌表格，放在首个相关段落的位置。
                let table = self
                    .cached_table_grid()
                    .map(|grid| (grid, self.table_line_set(state.lines.len())));
                let mut placed = false;
                for (index, rows) in state.paragraphs.iter().enumerate() {
                    if let Some((grid, lines)) = &table {
                        let inside = rows.clone().filter(|row| lines[*row]).count();
                        if inside > 0 && !placed {
                            placed = true;
                            body = body.child(
                                div()
                                    .flex_shrink_0()
                                    .py(px(4.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .px(px(8.))
                                            .text_size(px(11.5))
                                            .text_color(rgb(p.muted))
                                            .child(trf!(
                                                "表格 · {} 行 × {} 列",
                                                "Table · {} × {}",
                                                grid.rows(),
                                                grid.columns()
                                            )),
                                    )
                                    .child(self.render_table_grid(grid, p, cx)),
                            );
                        }
                        if inside == rows.len() {
                            continue;
                        }
                    }
                    let selected = rows
                        .clone()
                        .any(|row| overlay.filter.model.selected.contains(&row))
                        || selection.is_some_and(|s| {
                            rows.clone()
                                .any(|row| ocr_panel::row_selected(&state.lines[row], s))
                        });
                    let content = if overlay.filter.model.has_hidden() {
                        rows.clone()
                            .filter(|row| !overlay.filter.model.is_hidden(*row))
                            .filter_map(|row| {
                                state.lines.get(row).and_then(|r| text.get(r.clone()))
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into()
                    } else {
                        state.paragraph_text[index].clone()
                    };
                    let blank = content.trim().is_empty();
                    let row = row_base(("ocr-panel-paragraph", index), selected, hovered(rows))
                        .px(px(8.))
                        .py(px(5.))
                        .text_size(px(13.))
                        .line_height(px(21.))
                        .when(blank, |r| r.h(px(10.)))
                        .when(!blank, |r| r.child(content));
                    body = body.child(self.bind_ocr_panel_row(row, rows.clone(), window, cx));
                }
            }
            PanelView::Translation => {
                body = self.render_ocr_panel_translation_body(body, window, p, cx);
            }
            PanelView::Table => {
                body = match self.cached_table_grid() {
                    Some(grid) => {
                        let table = self.render_table_grid(grid, p, cx);
                        let source = match grid.source {
                            crate::table_grid::TableSource::Lines => tr!("线条", "Lines"),
                            crate::table_grid::TableSource::Alignment => tr!("对齐", "Alignment"),
                            crate::table_grid::TableSource::Legacy => tr!("文字网格", "Text grid"),
                            crate::table_grid::TableSource::Model => tr!("SLANet 模型", "SLANet model"),
                        };
                        let forced = if self.table_forced() {
                            tr!("（手动表格识别，请核对）", " (manual table, please check)")
                        } else {
                            ""
                        };
                        let region = if self.cached_table_region().is_some() {
                            tr!("正文中的表格，", "Table within text · ")
                        } else {
                            ""
                        };
                        body.child(table).child(
                            div()
                                .mx(px(6.))
                                .mt(px(8.))
                                .text_size(px(12.))
                                .text_color(rgb(p.muted))
                                .child(trf!(
                                    "{}{} 行 × {} 列，来源：{}{}；图上已描出单元格，复制保留合并单元格。",
                                    "{}{} rows × {} columns · {}{}; cells are outlined on the image, copy keeps merged cells.",
                                    region, grid.rows(), grid.columns(), source, forced
                                )),
                        )
                    }
                    None => body.child(
                        div()
                            .m(px(6.))
                            .p(px(10.))
                            .rounded_md()
                            .bg(rgb(p.hover))
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(self.cached_table_text().map_or_else(|| tr!(
                                "没有识别出完整表格，或正文已编辑。仍可在“段落”或“逐行”中复制。",
                                "No reliable table found, or the text was edited. Copy from Lines or Paragraphs instead."
                            ).to_string(), |text| trf!("保留原识别顺序，复制为 TSV：\n{}", "Kept in reading order; copy as TSV:\n{}", text))),
                    ),
                };
                body = body.when(self.table_force_available(), |body| {
                    body.child(
                        self.bind_ocr_panel_click(
                            div()
                                .id("ocr-table-force")
                                .mx(px(6.))
                                .h(px(30.))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(p.border))
                                .cursor_pointer()
                                .hover(move |s| s.bg(rgb(p.hover)))
                                .text_size(px(12.5))
                                .child(tr!("用模型识别表格", "Recognize table with model")),
                            "ocr-table-force",
                            PanelIntent::ForceTable,
                            window,
                            cx,
                        ),
                    )
                });
            }
        }
        body
    }

    fn bind_ocr_panel_row(
        &self,
        row: gpui::Stateful<gpui::Div>,
        rows: Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let hover_rows = rows.clone();
        let row = row.on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
            if *hovered {
                view.ocr_panel_set_hover(Some(hover_rows.clone()), false, cx);
            } else if view
                .ocr_overlay
                .as_ref()
                .is_some_and(|o| o.panel.hover.as_ref() == Some(&hover_rows))
            {
                view.ocr_panel_set_hover(None, false, cx);
            }
        }));
        self.bind_ocr_panel_row_down(row, rows, window, cx)
    }

    /// 译文页：全文按段落翻译时逐段对照（悬停高亮原图段落）；所选文字或段数不一致时整块显示。
    fn render_ocr_panel_translation_body(
        &self,
        mut body: gpui::Stateful<gpui::Div>,
        window: &Window,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use crate::translate::TranslationState;
        let overlay = self
            .ocr_overlay
            .as_ref()
            .expect("面板仅在原位文字层存在时渲染");
        let state = &overlay.panel;
        let note = |text: String| {
            div()
                .m(px(6.))
                .p(px(10.))
                .rounded_md()
                .bg(rgb(p.hover))
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(rgb(p.muted))
                .child(text)
        };
        let action = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .mx(px(6.))
                .h(px(30.))
                .px(px(12.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(rgb(p.border))
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(p.hover)))
                .text_size(px(12.5))
                .child(label)
        };
        let Some(translation) = self.current_translation() else {
            return body
                .child(note(
                    tr!(
                        "译文显示在这里。没有翻译服务时请在 设置 ▸ 翻译 中配置。",
                        "Translations appear here. Configure a service in Settings ▸ Translation."
                    )
                    .to_owned(),
                ))
                .child(self.bind_ocr_panel_click(
                    action("ocr-translate-start", tr!("翻译全文", "Translate all")),
                    "ocr-translate-start",
                    PanelIntent::TranslateAll,
                    window,
                    cx,
                ));
        };
        let translating = || {
            div()
                .px(px(10.))
                .pt(px(4.))
                .pb(px(8.))
                .text_size(px(12.))
                .text_color(rgb(p.muted))
                .child(tr!("正在翻译…", "Translating…"))
        };
        if let Some(grid) = &translation.table {
            return body.child(note(tr!("单元格译文 · 行列、合并和数值保持原样；复制时保留表格格式。", "Translated cells · rows, columns, merges and numbers are preserved. Copy keeps table formatting.").into()))
                .child(self.render_table_grid(grid, p, cx));
        }
        // 流式译文与完成结果同样逐段对照；已收到的段落先显示，末尾提示仍在翻译。
        let (text, streaming) = match &translation.state {
            TranslationState::Pending if translation.partial().is_empty() => {
                return body.child(translating()).child(skeleton(p));
            }
            TranslationState::Pending => (translation.partial(), true),
            TranslationState::Failed(error) => {
                return body
                    .child(note(error.clone()))
                    .child(self.bind_ocr_panel_click(
                        action("ocr-translate-retry", tr!("重新翻译", "Try again")),
                        "ocr-translate-retry",
                        PanelIntent::TranslateAll,
                        window,
                        cx,
                    ));
            }
            TranslationState::Done(text) => (text.as_str(), false),
        };
        if !streaming {
            if let Some(preview) = self.markdown_translation_preview(text, !crate::theme::is_light(cx)) {
                return body.child(crate::markdown_view::render(preview, p));
            }
        }
        let cards = self
            .translation_by_paragraph
            .then(|| translation_cards(state, text, streaming))
            .filter(|cards| !cards.is_empty());
        let body = if let Some(cards) = cards {
            // 开关在页签行行尾（见 render_ocr_panel）。
            let show_source = translation_show_source().load(std::sync::atomic::Ordering::Relaxed);
            for (
                index,
                TranslationCard {
                    rows,
                    source,
                    translated,
                },
            ) in cards.into_iter().enumerate()
            {
                let hot = state
                    .hover
                    .as_ref()
                    .is_some_and(|h| h.start < rows.end && rows.start < h.end);
                let card = div()
                    .id(("ocr-panel-translation-row", index))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded_md()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .when(hot, |r| r.bg(rgb(p.hover)))
                    .when(show_source, |card| {
                        card.child(
                            div()
                                .text_size(px(11.5))
                                .line_height(px(17.))
                                .text_color(rgb(p.muted))
                                .child(source),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(21.))
                            .child(translated),
                    );
                body = body.child(self.bind_ocr_panel_row(card, rows, window, cx));
            }
            body
        } else {
            body.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(px(13.))
                    .line_height(px(21.))
                    .child(text.to_owned()),
            )
        };
        body.when(streaming, |body| body.child(translating()))
    }

    fn render_ocr_panel_footer(
        &self,
        selection: Option<&Range<usize>>,
        window: &Window,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let overlay = self
            .ocr_overlay
            .as_ref()
            .expect("面板仅在原位文字层存在时渲染");
        let state = &overlay.panel;
        let layer = state.layer;
        let mut footer = div()
            .h(px(FOOTER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .border_t_1()
            .border_color(rgb(p.border))
            .child(
                self.bind_ocr_panel_click(
                    div()
                        .id("ocr-panel-layer")
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .whitespace_nowrap()
                        .child(mini_switch(layer, p))
                        .child(tr!("识别层", "Boxes"))
                        .tooltip(hint(tr!(
                            "在原图上显示识别到的文字行",
                            "Show recognized lines on the image"
                        ))),
                    "ocr-panel-layer",
                    PanelIntent::Layer(!layer),
                    window,
                    cx,
                ),
            );
        if self.ocr_sensitive_available() {
            // 底栏只有 340 DIP，入口只放图标，说明放在提示里，给复制按钮留出宽度。
            let chip = div()
                .id("ocr-sensitive-open")
                .size(px(28.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(rgb(p.border))
                .hover(move |s| s.bg(rgb(p.hover)))
                .child(toolbar_icons::action(Icon::Shield, p.foreground).size(px(14.)))
                .tooltip(hint(tr!(
                    "敏感文字：检查邮箱、电话和证件号，确认后打码",
                    "Sensitive text: review emails, phones and IDs, then mask them"
                )));
            footer = footer.child(self.bind_ocr_sensitive_button(
                chip,
                SensitiveIntent::Open,
                window,
                cx,
            ));
        }
        if matches!(state.view, PanelView::Lines | PanelView::Paragraphs) {
            let active = overlay.filter.model.auto_filter;
            footer = footer.child(self.bind_ocr_panel_click(
                icon_button(
                    "ocr-filter-open",
                    Icon::Filter,
                    if active {
                        tr!("过滤：已开启", "Filter: on")
                    } else {
                        tr!("过滤", "Filter")
                    },
                    active || state.filter_open,
                    p,
                ),
                "ocr-filter-open",
                PanelIntent::FilterMenu(!state.filter_open),
                window,
                cx,
            ));
        }
        footer = footer.child(div().flex_1());
        if state.view == PanelView::Translation {
            let done = self.translation_copy_ticket();
            let ticket = done.clone();
            let mut button = primary_button("ocr-panel-copy-translation", p)
                .child(toolbar_icons::action(Icon::Copy, p.background).size(px(15.)))
                .child(tr!("复制译文", "Copy translation"));
            button = if done.is_some() {
                button
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event, window, cx| {
                            view.begin_translation_copy_pointer(ticket.clone(), event, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_click(cx.listener(|view, event, window, cx| {
                        view.copy_translation_pointer(event, window, cx)
                    }))
            } else {
                button.opacity(0.45)
            };
            return footer.child(button);
        }
        if state.view == PanelView::Table && selection.is_none() {
            // 表格复制沿用既有按下／松开回执，外层只提供主按钮外观。
            return footer.child(
                primary_button("ocr-panel-copy-table", p)
                    .child(toolbar_icons::action(Icon::Copy, p.background).size(px(15.)))
                    .child(self.render_table_copy_button(self.result_output_frame(), true, cx)),
            );
        }
        let has_text =
            !overlay.text.text.trim().is_empty() && overlay.filter.model.kept_count() > 0;
        let (mut label, mut operation): (SharedString, Operation) = match (selection, state.view) {
            (Some(_), view) => (
                tr!("复制所选", "Copy selected").into(),
                if view == PanelView::Paragraphs {
                    Operation::CopyParagraphs
                } else {
                    Operation::CopyText
                },
            ),
            // 当前视图已标明是段落，按钮文案统一为“复制全文”，英文也不会撑出底栏。
            (None, view) => (
                tr!("复制全文", "Copy all").into(),
                if view == PanelView::Paragraphs {
                    Operation::CopyFullParagraphs
                } else {
                    Operation::CopyFull
                },
            ),
        };
        if overlay.filter.lines_selected() {
            label = trf!(
                "复制所选 {}",
                "Copy selected {}",
                overlay.filter.model.selected_count()
            )
            .into();
            operation = Operation::CopyText;
        } else if selection.is_none() && overlay.filter.model.has_hidden() {
            label = tr!("复制保留项", "Copy kept").into();
            operation = Operation::CopyText;
        }
        footer.child(
            primary_button("ocr-panel-copy", p)
                // 极窄面板时按钮可收缩，文字截断而不是被面板边缘裁掉。
                .flex_shrink()
                .min_w(px(0.))
                .overflow_hidden()
                .child(
                    toolbar_icons::action(Icon::Copy, p.background)
                        .size(px(15.))
                        .flex_shrink_0(),
                )
                .child(div().min_w(px(0.)).truncate().child(label))
                .when(!has_text, |b| b.opacity(0.45))
                .when(has_text, |b| {
                    self.bind_ocr_panel_click(
                        b.cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .tooltip(hint(tr!(
                                "复制后关闭截图；Ctrl+C 复制后保持打开",
                                "Copy and close; Ctrl+C copies and stays open"
                            ))),
                        "ocr-panel-copy",
                        PanelIntent::CopyAndClose(operation),
                        window,
                        cx,
                    )
                }),
        )
    }

    fn render_ocr_panel_menu(
        &self,
        window: &Window,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let pending = self.translation_pending();
        let retry = self.result_retry_available();
        let item = |id: &'static str, label: &'static str, key: &'static str, enabled: bool| {
            div()
                .id(id)
                .h(px(30.))
                .px(px(10.))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .rounded_sm()
                .text_size(px(13.))
                .whitespace_nowrap()
                .text_color(rgb(if enabled { p.foreground } else { p.muted }))
                .when(enabled, |i| {
                    i.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
                })
                .when(!enabled, |i| {
                    i.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                })
                .child(label)
                .child(div().text_size(px(11.)).text_color(rgb(p.muted)).child(key))
        };
        div()
            .absolute()
            .top(px(HEADER_HEIGHT - 6.))
            .right(px(8.))
            .w(px(220.))
            .p(px(4.))
            .flex()
            .flex_col()
            .rounded_md()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .shadow_lg()
            .child(
                item(
                    "ocr-menu-translate",
                    if pending {
                        tr!("正在翻译…", "Translating…")
                    } else if self.ocr_overlay.as_ref().is_some_and(|overlay| {
                        overlay.selected.as_ref().is_some_and(|r| !r.is_empty())
                    }) {
                        tr!("翻译所选", "Translate selection")
                    } else {
                        tr!("翻译全文", "Translate all")
                    },
                    "",
                    !pending,
                )
                .when(!pending, |i| {
                    self.bind_ocr_panel_click(
                        i,
                        "ocr-menu-translate",
                        PanelIntent::Operation(Operation::Translate),
                        window,
                        cx,
                    )
                }),
            )
            .child(
                item(
                    "ocr-menu-translate-image",
                    if self.image_translation.busy() {
                        tr!("正在翻译图片…", "Translating image…")
                    } else {
                        tr!("翻译图片", "Translate image")
                    },
                    "Y",
                    !self.image_translation.busy(),
                )
                .when(!self.image_translation.busy(), |i| {
                    self.bind_ocr_panel_click(
                        i,
                        "ocr-menu-translate-image",
                        PanelIntent::Operation(Operation::TranslateImage),
                        window,
                        cx,
                    )
                }),
            )
            .child(
                item(
                    "ocr-menu-table",
                    tr!("识别为表格", "Recognize as table"),
                    "",
                    self.table_force_available(),
                )
                .when(self.table_force_available(), |item| {
                    self.bind_ocr_panel_click(
                        item,
                        "ocr-menu-table",
                        PanelIntent::ForceTable,
                        window,
                        cx,
                    )
                }),
            )
            .child(
                item(
                    "ocr-menu-retry",
                    if self.result_retry_busy() {
                        tr!("正在重新识别…", "Recognizing again…")
                    } else {
                        tr!("重新识别全部", "Recognize again")
                    },
                    "",
                    retry,
                )
                .when(retry, |i| {
                    self.bind_ocr_panel_click(i, "ocr-menu-retry", PanelIntent::Retry, window, cx)
                }),
            )
            .child(self.bind_ocr_panel_click(
                item(
                    "ocr-menu-image",
                    tr!("复制图片", "Copy image"),
                    "Enter",
                    true,
                ),
                "ocr-menu-image",
                PanelIntent::Operation(Operation::CopyImage),
                window,
                cx,
            ))
    }

    /// 选中文字旁的标记条：复制、四种标记与标记色。
    pub(super) fn render_ocr_mark_bar(
        &self,
        monitor: PanelRect,
        selected: PanelRect,
        markable: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (x, y) = self.ocr_mark_bar_position(monitor, selected, window)?;
        Some(
            self.render_ocr_mark_controls(markable, window, cx)?
                .id("ocr-mark-bar")
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(MARK_BAR_WIDTH))
                .h(px(MARK_BAR_HEIGHT))
                .into_any_element(),
        )
    }

    /// 浮栏无空位时复用同一批控件，放在结果面板标题下；窄区换行、极小区可滚动。
    fn render_ocr_panel_mark_controls(
        &self,
        layout: PanelLayout,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let overlay = self.ocr_overlay.as_ref()?;
        let range = overlay.selected.clone().filter(|range| !range.is_empty())?;
        if overlay.anchor.is_some() {
            return None;
        }
        let projection = self.ocr_projection(window)?;
        let mark_quads = overlay.mark_selection_quads().ok();
        let selected = mark_quads
            .as_deref()
            .and_then(super::quad_bounds)
            .or_else(|| super::quad_bounds(&overlay.text.highlights(range)));
        let selected = if let Some(bounds) = selected {
            projection.bounds(bounds)
        } else {
            projection.rect(overlay.target.selected)
        };
        let work = self.ocr_ui_work(window)?;
        if self.ocr_mark_bar_position(work, selected, window).is_some() {
            return None;
        }
        let controls = self
            .render_ocr_mark_controls(mark_quads.is_some(), window, cx)?
            .id("ocr-panel-mark-tools")
            .w_full()
            .min_w(px(MARK_BUTTON + 10.))
            .max_w(px(MARK_BAR_WIDTH))
            .mx_auto()
            .flex_shrink_0()
            .flex_wrap();
        Some(
            div()
                .id("ocr-panel-mark-tools-host")
                .px(px(8.))
                .py(px(4.))
                .flex()
                .flex_shrink_0()
                .max_h(px((layout.panel.height - HEADER_HEIGHT - 8.).max(1.)))
                .overflow_x_scroll()
                .overflow_y_scroll()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .child(controls)
                .into_any_element(),
        )
    }

    /// 与真正绘制的结果面板、一级栏和当前属性栏使用同一几何与显示门。
    /// 面板回退也调用此方法，不能只在一个入口拒绝浮栏而让另一入口丢控件。
    fn ocr_mark_bar_position(
        &self,
        work: PanelRect,
        selected: PanelRect,
        window: &Window,
    ) -> Option<(f32, f32)> {
        let mut obstacles = self
            .ocr_panel_frame(window)
            .map(|layout| vec![layout.panel])
            .unwrap_or_default();
        if !self.pin_mode
            && (self.image_canvas.is_some() || self.screenshot_toolbar_is_shown())
            && !self.editor.is_changing_crop()
        {
            if let Ok(crop) = self.selected() {
                let layout = self.toolbar_layout(crop, window.scale_factor());
                obstacles.push(layout.primary);
                let tool = self
                    .editor
                    .selected_annotation()
                    .map_or(self.editor.tool, |a| a.tool);
                if (tool == crate::annotation::Tool::Crop && self.image_canvas.is_some())
                    || tool.draws()
                    || matches!(
                        tool,
                        crate::annotation::Tool::Lasso
                            | crate::annotation::Tool::Eraser
                            | crate::annotation::Tool::SmartMosaic
                    )
                {
                    obstacles.push(layout.properties);
                }
            }
        }
        ocr_panel::mark_bar_origin_avoiding(
            work,
            selected,
            MARK_BAR_WIDTH,
            MARK_BAR_HEIGHT,
            &obstacles,
        )
    }

    fn render_ocr_mark_controls(
        &self,
        markable: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let overlay = self.ocr_overlay.as_ref()?;
        let p = crate::theme::palette(cx);
        let selected_quads = markable
            .then(|| overlay.mark_selection_quads().ok())
            .flatten();
        let separator = || {
            div()
                .w(px(1.))
                .h(px(18.))
                .mx(px(4.))
                .flex_shrink_0()
                .bg(rgb(p.border))
        };
        let mut bar = div()
            .p(px(4.))
            .flex()
            .items_center()
            .gap(px(2.))
            .rounded_lg()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .shadow_md()
            .text_color(rgb(p.foreground))
            .cursor(gpui::CursorStyle::Arrow)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                self.bind_ocr_panel_click(
                    div()
                        .id("ocr-mark-copy")
                        .size(px(MARK_BUTTON))
                        .h(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(move |s| s.bg(rgb(p.hover)))
                        .child(toolbar_icons::action(Icon::Copy, p.foreground).size(px(16.)))
                        .tooltip(hint(tr!("复制所选 · Ctrl+C", "Copy selected · Ctrl+C"))),
                    "ocr-mark-copy",
                    PanelIntent::Operation(Operation::CopyText),
                    window,
                    cx,
                ),
            )
            .child(
                self.bind_ocr_panel_click(
                    div()
                        .id("ocr-mark-translate")
                        .size(px(MARK_BUTTON))
                        .h(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(move |s| s.bg(rgb(p.hover)))
                        .child(toolbar_icons::action(Icon::Translate, p.foreground).size(px(16.)))
                        .tooltip(hint(tr!(
                            "翻译所选 · 结果在面板“译文”页",
                            "Translate selection · shown in the Translation tab"
                        ))),
                    "ocr-mark-translate",
                    PanelIntent::Operation(Operation::Translate),
                    window,
                    cx,
                ),
            )
            .child(separator());
        for (id, mark, tip) in [
            (
                "ocr-highlight",
                TextMark::Highlight,
                tr!("高亮 · H", "Highlight · H"),
            ),
            (
                "ocr-underline",
                TextMark::Underline,
                tr!("下划线 · U", "Underline · U"),
            ),
            (
                "ocr-wave",
                TextMark::Wave,
                tr!("波浪线 · W", "Wavy line · W"),
            ),
            (
                "ocr-strike",
                TextMark::Strike,
                tr!("删除线 · D", "Strikethrough · D"),
            ),
        ] {
            let tip = if markable {
                tip
            } else {
                tr!(
                    "标记需要完整文字定位和可用输入；复制不需要文字定位",
                    "Needs full text positions and ready input; copy needs no positions"
                )
            };
            let color = overlay.color.unwrap_or(if mark == TextMark::Highlight {
                MARK_COLORS[0]
            } else {
                self.editor.style.color
            });
            let active = selected_quads.as_ref().is_some_and(|quads| {
                overlay
                    .ledger
                    .selection_fully_marked(
                        &self.editor.document,
                        mark,
                        quads,
                        crate::annotation::Style {
                            color,
                            ..self.editor.style
                        },
                    )
                    .unwrap_or(false)
            });
            bar = bar.child(
                div()
                    .id(id)
                    .w(px(MARK_BUTTON))
                    .h(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(!markable, |b| b.opacity(0.4))
                    .when(active, |b| b.bg(rgb(p.selected)).text_color(rgb(p.accent)))
                    .child(toolbar_icons::text_mark(mark, color))
                    .tooltip(hint(tip))
                    .when(markable, |b| {
                        self.bind_ocr_panel_click(
                            b.cursor_pointer().hover(move |s| {
                                s.bg(rgb(if active { p.selected } else { p.hover }))
                            }),
                            id,
                            PanelIntent::Operation(Operation::Mark(mark)),
                            window,
                            cx,
                        )
                    }),
            );
        }
        bar = bar.child(separator());
        for (id, color) in [
            ("ocr-yellow", MARK_COLORS[0]),
            ("ocr-red", MARK_COLORS[1]),
            ("ocr-blue", MARK_COLORS[2]),
            ("ocr-green", MARK_COLORS[3]),
        ] {
            let chosen = overlay.color == Some(color);
            bar = bar.child(
                self.bind_ocr_panel_click(
                    div()
                        .id(id)
                        .w(px(MARK_DOT))
                        .h(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child(
                            div()
                                .size(px(14.))
                                .rounded_full()
                                .bg(rgb(color))
                                .border_2()
                                .border_color(rgb(if chosen {
                                    p.foreground
                                } else {
                                    p.background
                                })),
                        )
                        .tooltip(hint(tr!("标记色", "Mark color"))),
                    id,
                    PanelIntent::Operation(Operation::Color(color)),
                    window,
                    cx,
                ),
            );
        }
        Some(bar)
    }
}

#[cfg(test)]
mod panel_click_state_tests {
    use super::*;

    fn state() -> PanelState {
        PanelState {
            view: PanelView::Lines,
            hover: None,
            layer: true,
            menu_open: false,
            filter_open: false,
            filter_scroll: ScrollHandle::new(),
            lines: std::iter::once(0..1).collect(),
            line_quads: vec![Vec::new()],
            paragraphs: std::iter::once(0..1).collect(),
            paragraph_text: vec!["a".into()],
            scroll: ScrollHandle::new(),
            intent_epoch: Arc::new(()),
        }
    }

    #[test]
    fn menu_aba_preserves_visible_value_but_replaces_down_epoch() {
        let mut panel = state();
        let (before, context) = panel.click_state();
        panel.set_menu(true);
        panel.close_menu();
        let (after, restored) = panel.click_state();
        assert!(context == restored);
        assert!(!Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn filter_popup_keeps_body_view_and_scroll_and_is_exclusive_with_menu() {
        let mut panel = state();
        panel.set_view(PanelView::Paragraphs);
        panel.scroll.set_offset(point(px(0.), px(-48.)));
        panel.set_menu(true);
        panel.set_filter_open(true);
        assert!(panel.filter_open());
        assert!(!panel.menu_open());
        assert_eq!(panel.view(), PanelView::Paragraphs);
        assert_eq!(panel.scroll.offset(), point(px(0.), px(-48.)));
        panel.set_menu(true);
        assert!(!panel.filter_open());
        panel.set_filter_open(true);
        panel.set_view(PanelView::Lines);
        assert!(!panel.filter_open());
        assert!(!panel.menu_open());
    }

    #[test]
    fn filter_popup_aba_and_scrolling_reject_stale_click_context() {
        let mut panel = state();
        let (before, context) = panel.click_state();
        panel.set_filter_open(true);
        panel.close_menu();
        let (after, restored) = panel.click_state();
        assert!(context == restored);
        assert!(!Arc::ptr_eq(&before, &after));
        panel.set_filter_open(true);
        let (_, before_scroll) = panel.click_state();
        panel.filter_scroll.set_offset(point(px(0.), px(-32.)));
        assert!(before_scroll != panel.click_state().1);
    }

    #[test]
    fn layer_and_view_aba_each_replace_real_panel_epoch() {
        let mut panel = state();
        let (before, context) = panel.click_state();
        panel.set_layer(false);
        panel.set_layer(true);
        let (layer, restored) = panel.click_state();
        assert!(context == restored);
        assert!(!Arc::ptr_eq(&before, &layer));
        panel.set_view(PanelView::Paragraphs);
        panel.set_view(PanelView::Lines);
        let (view, restored) = panel.click_state();
        assert!(context == restored);
        assert!(!Arc::ptr_eq(&layer, &view));
    }

    #[test]
    fn unchanged_panel_values_keep_epoch_and_view_change_resets_scroll_and_menu() {
        let mut panel = state();
        let (before, _) = panel.click_state();
        panel.set_menu(false);
        panel.set_layer(true);
        panel.set_view(PanelView::Lines);
        assert!(Arc::ptr_eq(&before, &panel.click_state().0));
        panel.scroll.set_offset(point(px(-4.5), px(-8.25)));
        panel.set_menu(true);
        panel.set_view(PanelView::Table);
        assert_eq!(panel.scroll.offset(), point(px(0.), px(0.)));
        assert!(!panel.menu_open());
        assert!(!Arc::ptr_eq(&before, &panel.click_state().0));
    }

    #[test]
    fn actual_scroll_handle_offset_is_part_of_render_intent() {
        let panel = state();
        let (_, before) = panel.click_state();
        panel.scroll.set_offset(point(px(-4.5), px(-8.25)));
        assert!(before != panel.click_state().1);
        // 内部 A→B→A 的历史由 paint 中的 capture wheel 取消槽，不借相等 offset 放行。
        panel.scroll.set_offset(point(px(0.), px(0.)));
        assert!(before == panel.click_state().1);
    }
}

#[cfg(test)]
mod strip_fit_tests {
    use super::*;

    #[test]
    fn ordinary_text_tabs_stay_text_until_table_is_requested() {
        let plain = panel_tabs(false);
        assert_eq!(plain.len(), 3);
        assert!(!plain.iter().any(|(_, _, view)| *view == PanelView::Table));
        assert!(plain
            .iter()
            .any(|(_, _, view)| *view == PanelView::Translation));
        let table = panel_tabs(true);
        assert_eq!(table.len(), 4);
        assert!(table.iter().any(|(_, _, view)| *view == PanelView::Table));
    }

    #[test]
    fn strip_fit_degrades_by_measured_width_not_language() {
        // 四个短页签（中文量级）：带文字的“对照”放得下。
        assert_eq!(strip_fit(338., &[27.; 4], 25.), StripFit::Full);
        // 英文量级（约 73/34/44/85）：文字按钮放不下，只留图标。
        let long = [73., 34., 44., 85.];
        assert_eq!(strip_fit(338., &long, 45.), StripFit::IconChip);
        // 更窄或更长的翻译：页签自身也放不下时允许压缩截断。
        assert_eq!(strip_fit(280., &long, 45.), StripFit::Squeeze);
        // 恰好放下不算溢出。
        let exact = 27. * 4. + TAB_PAD * 8. + STRIP_PAD * 2. + STRIP_GAP * 4. + CHIP_MARGIN;
        assert_eq!(
            strip_fit(exact + CHIP_CHROME + 25., &[27.; 4], 25.),
            StripFit::Full
        );
    }
}
