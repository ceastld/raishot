//! 二级栏：按 property_items 的清单与收起级别逐项绘制共用控件；修改仍交给 Editor 的增量事务。
use crate::{
    annotation::{HighlighterBlend, HighlighterShape, Style, Tool},
    arrow_geometry::{self, ArrowEnds, ArrowHead},
    capture_toolbar::Palette,
    capture_view::{Action, CaptureView},
    interaction::{DrawingEffect, Parameter},
    property_items::{self as items, Hint, Item, PropertyContext, Slot},
    text_format::Alignment,
    toolbar_icons::Icon,
    toolbar_layout::{ChoiceKind, PanelRect, ToolbarLayout, ToolbarPanel},
    toolbar_widgets::{self as w, Lead, State},
};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context};

/// 色块提示用的颜色名。
fn swatch_name(id: &str) -> &'static str {
    match id {
        "red" => tr!("红", "Red"),
        "orange" => tr!("橙", "Orange"),
        "yellow" => tr!("黄", "Yellow"),
        "green" => tr!("绿", "Green"),
        "blue" => tr!("蓝", "Blue"),
        "purple" => tr!("紫", "Purple"),
        "white" => tr!("白", "White"),
        _ => tr!("黑", "Black"),
    }
}

/// 数值文字：整数不带小数点。
pub(crate) fn value_label(value: f32) -> String {
    let label = format!("{value:.1}");
    label.strip_suffix(".0").unwrap_or(&label).to_owned()
}

/// 数值框的滚轮参数；角度没有滚轮档位。
fn wheel_parameter(panel: ToolbarPanel) -> Option<Parameter> {
    match panel {
        ToolbarPanel::Width => Some(Parameter::Width),
        ToolbarPanel::Font => Some(Parameter::Font),
        ToolbarPanel::Radius => Some(Parameter::Radius),
        ToolbarPanel::Opacity => Some(Parameter::Opacity),
        ToolbarPanel::Block => Some(Parameter::Block),
        ToolbarPanel::TextOutline => Some(Parameter::TextOutline),
        ToolbarPanel::TextBackgroundAlpha => Some(Parameter::TextBackgroundAlpha),
        ToolbarPanel::TextBackgroundRadius => Some(Parameter::TextBackgroundRadius),
        _ => None,
    }
}

/// “样式”弹层的行标签。
pub(crate) fn item_label(item: Item) -> &'static str {
    match item {
        Item::FillMode | Item::FillMenu => tr!("填充", "Fill"),
        Item::LineStyle | Item::LineStyleMenu => tr!("线型", "Line"),
        Item::ArrowEnds | Item::ArrowMenu => tr!("箭头", "Arrow"),
        Item::Straighten => tr!("拉直", "Straight"),
        Item::PolylineSmooth | Item::PenSmooth => tr!("平滑", "Smooth"),
        Item::PolylineClosed => tr!("闭合", "Closed"),
        Item::DeleteNode => tr!("顶点", "Point"),
        Item::Blend | Item::BlendMenu => tr!("叠加", "Blend"),
        Item::Stroke | Item::StrokeMenu => tr!("笔迹", "Stroke"),
        Item::Radius => tr!("圆角", "Radius"),
        Item::FontFamily => tr!("字体", "Font"),
        Item::Glyphs | Item::BoldOnly | Item::TextStyleMenu => tr!("字形", "Style"),
        Item::Align => tr!("对齐", "Align"),
        Item::TextOutline => tr!("描边", "Outline"),
        Item::TextBackground => tr!("底色", "Fill"),
        Item::SerialBold => tr!("粗体", "Bold"),
        Item::WatermarkAngle | Item::Angle => tr!("角度", "Angle"),
        Item::Effect | Item::SmartEffect => tr!("效果", "Effect"),
        Item::Region => tr!("区域", "Area"),
        Item::Smart => tr!("智能", "Smart"),
        _ => "",
    }
}

impl CaptureView {
    /// 实际选中上下文；布局用同一工具的保守估计，这里只会更窄。
    pub(crate) fn property_context(&self, tool: Tool) -> PropertyContext {
        let count = self.editor.selected_ids().len();
        let selected = self.editor.selected_annotation();
        let radius = self.editor.radius_selection();
        PropertyContext {
            tool,
            count,
            radius_targets: radius.targets.len(),
            // 线条方向由端点决定、序号是圆，单独选中时不给角度入口。
            rotatable: count > 1
                || selected.is_some_and(|a| {
                    !matches!(
                        a.tool,
                        Tool::Arrow | Tool::Polyline | Tool::Serial | Tool::Watermark
                    )
                }),
            edit_text: tool == Tool::Text
                && self.editor.text_draft.is_none()
                && selected.is_some_and(|a| a.tool.text()),
            curved_arrow: tool == Tool::Arrow && selected.is_some_and(|a| a.points.len() >= 3),
            radius: !radius.targets.is_empty()
                || (radius.selected_count == 0 && tool.supports_radius()),
            text_draft: self.editor.text_draft.is_some(),
        }
    }

    pub(crate) fn property_toolbar(
        &self,
        tool: Tool,
        rect: PanelRect,
        level: u8,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 贴图浮窗仍使用既有由上往下的排列。
        self.property_toolbar_direction(tool, rect, false, level, cx)
    }

    /// 二级栏：按组换行的多行，组间竖线；在主栏上方时第一行贴近主栏。
    pub(crate) fn property_toolbar_direction(
        &self,
        tool: Tool,
        rect: PanelRect,
        above: bool,
        level: u8,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        self.property_level.set(level);
        let ctx = self.property_context(tool);
        let list = items::items(&ctx, level);
        let rows = items::pack(&list, (rect.width - items::BAR_INSET).max(32.));
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(items::ROW_GAP))
            .text_size(px(12.))
            .when(above, |body| body.flex_col_reverse().h_full());
        for row in rows {
            let mut line = div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(items::ITEM_GAP))
                .h(px(items::ROW_HEIGHT));
            for slot in row {
                line = match slot {
                    Slot::Separator => line.child(w::separator(p)),
                    Slot::Item(index) => line.child(self.property_item(list[index].1, &ctx, cx)),
                };
            }
            body = body.child(line);
        }
        self.panel("property-toolbar", rect)
            .rounded(px(10.))
            .p(px(4.))
            .child(body)
            .child(self.probe("property-toolbar"))
            .into_any_element()
    }

    /// 改样式并提交一次增量事务；没有变化不提交。
    fn style_listener(
        &self,
        update: impl Fn(&mut Style) + 'static,
        cx: &mut Context<Self>,
    ) -> Box<dyn Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App)> {
        Box::new(cx.listener(move |v, _, _, cx| {
            let mut s = v.editor.style;
            update(&mut s);
            if s != v.editor.style {
                v.editor.set_style(s);
            }
            cx.notify();
        }))
    }

    /// 数值框：单击开弹层，悬停滚轮调值（Shift 加快，连续滚动合成一次撤销）。
    #[allow(clippy::too_many_arguments)]
    fn value_item(
        &self,
        id: &'static str,
        lead: Lead,
        value: String,
        reserve: f32,
        panel: ToolbarPanel,
        title: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let wheel = wheel_parameter(panel);
        w::value_field(id, lead, value, reserve, self.toolbar_panel == panel, p)
            .tooltip(w::tip(
                title,
                "",
                if wheel.is_some() {
                    tr!("单击设置，滚轮调整", "Click to set, scroll to adjust")
                } else {
                    ""
                },
            ))
            .on_scroll_wheel(cx.listener(move |v, e, w, cx| {
                if let Some(parameter) = wheel {
                    v.wheel_parameter(parameter, e, w, cx);
                }
                cx.stop_propagation();
            }))
            .on_click(cx.listener(move |v, _, w, cx| v.perform(Action::Panel(panel), w, cx)))
            .child(self.probe(id))
            .into_any_element()
    }

    fn icon_item(
        &self,
        id: &'static str,
        icon: Icon,
        state: State,
        tip: (&'static str, &'static str, &'static str),
        action: Action,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let enabled = !state.disabled;
        w::icon_button(id, icon, state, p)
            .tooltip(w::tip(tip.0, tip.1, tip.2))
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.perform(action, w, cx);
                }
            }))
            .child(self.probe(id))
            .into_any_element()
    }

    fn toggle_item(
        &self,
        id: &'static str,
        icon: Icon,
        on: bool,
        tip: (&'static str, &'static str),
        update: fn(&mut Style),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        w::icon_button(id, icon, State::on(on), p)
            .tooltip(w::tip(tip.0, "", tip.1))
            .on_click(self.style_listener(update, cx))
            .child(self.probe(id))
            .into_any_element()
    }

    fn dropdown_item(
        &self,
        id: &'static str,
        icon: Icon,
        panel: ToolbarPanel,
        state: State,
        tip: (&'static str, String),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let enabled = !state.disabled;
        let state = State {
            open: self.toolbar_panel == panel,
            ..state
        };
        w::dropdown_icon(id, icon, state, p)
            .tooltip(w::tip(tip.0, "", tip.1))
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.perform(Action::Panel(panel), w, cx)
                }
            }))
            .child(self.probe(id))
            .into_any_element()
    }

    /// 单选分段：每段（id、图标、提示、是否选中、不可用原因、点击）。
    fn segment_row(
        &self,
        segments: Vec<(
            &'static str,
            Icon,
            &'static str,
            bool,
            Option<&'static str>,
            Box<dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>)>,
        )>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let mut track = w::segments();
        for (id, icon, title, on, why, click) in segments {
            let disabled = why.is_some();
            let click = std::rc::Rc::new(click);
            track = track.child(
                w::segment(id, icon, State::on(on).disabled(disabled), p)
                    .tooltip(w::tip(title, "", why.unwrap_or("")))
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if !disabled {
                            click(v, w, cx);
                            cx.notify();
                        }
                    }))
                    .child(self.probe(id)),
            );
        }
        w::track(track, p).into_any_element()
    }

    /// 改样式的单选分段（无不可用项）。
    fn style_segments(
        &self,
        segments: Vec<(&'static str, Icon, &'static str, bool, fn(&mut Style))>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.segment_row(
            segments
                .into_iter()
                .map(|(id, icon, title, on, update)| {
                    (
                        id,
                        icon,
                        title,
                        on,
                        None,
                        Box::new(
                            move |v: &mut CaptureView,
                                  _: &mut gpui::Window,
                                  _: &mut Context<CaptureView>| {
                                let mut s = v.editor.style;
                                update(&mut s);
                                if s != v.editor.style {
                                    v.editor.set_style(s);
                                }
                            },
                        )
                            as Box<
                                dyn Fn(
                                    &mut CaptureView,
                                    &mut gpui::Window,
                                    &mut Context<CaptureView>,
                                ),
                            >,
                    )
                })
                .collect(),
            cx,
        )
    }

    fn swatch_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let current = self.editor.style.color & 0xff_ffff;
        let known = items::SWATCHES.iter().any(|(_, c)| *c == current);
        let mut row = div().flex().flex_shrink_0().items_center();
        for (id, color) in items::SWATCHES {
            row = row.child(
                w::swatch(id, color, current == color, p)
                    .tooltip(w::tip(swatch_name(id), "", ""))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.editor.set_color(color);
                        cx.notify();
                    }))
                    .child(self.probe(id)),
            );
        }
        let note = if known {
            tr!("打开取色器", "Open color picker").to_owned()
        } else {
            trf!("当前 #{:06X}", "Current #{:06X}", current)
        };
        row.child(
            w::custom_swatch("custom-color", (!known).then_some(current), p)
                .tooltip(w::tip(tr!("自定义颜色", "Custom color"), "", note))
                .on_click(cx.listener(|v, _, w, cx| v.perform(Action::PickColor, w, cx)))
                .child(self.probe("custom-color")),
        )
        .into_any_element()
    }

    fn redaction_effect(&self, ctx: &PropertyContext, cx: &mut Context<Self>) -> AnyElement {
        let tool = ctx.tool;
        let current = DrawingEffect::of(tool);
        let brush_mode = self.editor.effect_brush_mode;
        let segments = [
            (
                "effect-mosaic",
                Icon::Mosaic,
                tr!("马赛克", "Mosaic"),
                DrawingEffect::Mosaic,
            ),
            (
                "effect-blur",
                Icon::Blur,
                tr!("模糊", "Blur"),
                DrawingEffect::Blur,
            ),
            (
                "effect-local-fill",
                Icon::LocalFill,
                tr!("填充 · 手绘去除内容", "Fill · paint to remove content"),
                DrawingEffect::LocalFill,
            ),
        ]
        .into_iter()
        .map(|(id, icon, title, effect)| {
            // 已框选的对象是矩形区域，不能改成只能涂抹的填充。
            let why = (effect == DrawingEffect::LocalFill
                && ctx.count > 0
                && matches!(tool, Tool::Mosaic | Tool::Blur))
            .then_some(tr!(
                "框选的对象不能改成填充",
                "A box region can't become fill"
            ));
            let next = effect.next_tool(tool, brush_mode);
            (
                id,
                icon,
                title,
                current == Some(effect),
                why,
                Box::new(
                    move |v: &mut CaptureView,
                          w: &mut gpui::Window,
                          cx: &mut Context<CaptureView>| {
                        v.perform(Action::Tool(next), w, cx)
                    },
                )
                    as Box<dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>)>,
            )
        })
        .collect();
        self.segment_row(segments, cx)
    }

    fn redaction_region(&self, ctx: &PropertyContext, cx: &mut Context<Self>) -> AnyElement {
        let tool = ctx.tool;
        let brush = tool.freehand();
        let blur = matches!(tool, Tool::Blur | Tool::BlurBrush);
        let segments = [
            (
                "region-rect",
                Icon::RegionRect,
                tr!("框选", "Box"),
                false,
                if blur { Tool::Blur } else { Tool::Mosaic },
            ),
            (
                "region-brush",
                Icon::MosaicBrush,
                tr!("涂抹", "Brush"),
                true,
                if blur {
                    Tool::BlurBrush
                } else {
                    Tool::MosaicBrush
                },
            ),
        ]
        .into_iter()
        .map(|(id, icon, title, is_brush, next)| {
            let why = if is_brush == brush {
                None
            } else if tool == Tool::LocalFill {
                Some(tr!("填充只能涂抹", "Fill is brush-only"))
            } else if ctx.count > 0 {
                Some(tr!(
                    "已画的对象不能切换区域",
                    "Drawn objects keep their area"
                ))
            } else {
                None
            };
            (
                id,
                icon,
                title,
                is_brush == brush,
                why,
                Box::new(
                    move |v: &mut CaptureView,
                          w: &mut gpui::Window,
                          cx: &mut Context<CaptureView>| {
                        v.perform(Action::Tool(next), w, cx)
                    },
                )
                    as Box<dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>)>,
            )
        })
        .collect();
        self.segment_row(segments, cx)
    }

    fn arrow_ends(&self, cx: &mut Context<Self>) -> AnyElement {
        let style = self.editor.style;
        let ends = ArrowEnds::of(style);
        let head = style.arrow_head;
        let boxed = |f: Box<dyn Fn(&mut Style)>| {
            Box::new(
                move |v: &mut CaptureView, _: &mut gpui::Window, _: &mut Context<CaptureView>| {
                    let mut s = v.editor.style;
                    f(&mut s);
                    if s != v.editor.style {
                        v.editor.set_style(s);
                    }
                },
            )
                as Box<dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>)>
        };
        let end_segments = [
            (
                "arrow-ends-none",
                Icon::EndNone,
                tr!("无箭头", "No arrow"),
                ArrowEnds::None,
            ),
            (
                "arrow-ends-single",
                Icon::EndSingle,
                tr!("单箭头", "Single"),
                ArrowEnds::Single,
            ),
            (
                "arrow-ends-double",
                Icon::EndBoth,
                tr!("双箭头", "Double"),
                ArrowEnds::Both,
            ),
        ]
        .into_iter()
        .map(|(id, icon, title, value)| {
            let why =
                (value == ArrowEnds::Both && head == ArrowHead::Dart && ends != ArrowEnds::None)
                    .then_some(tr!("尖尾箭头只能单向", "Dart arrows are one-way"));
            (
                id,
                icon,
                title,
                ends == value,
                why,
                boxed(Box::new(move |s: &mut Style| value.apply(s))),
            )
        })
        .collect();
        let head_segments = [
            (
                "arrow-head-open",
                Icon::HeadOpen,
                tr!("开口", "Open"),
                ArrowHead::Open,
            ),
            (
                "arrow-head-triangle",
                Icon::HeadTriangle,
                tr!("实心", "Solid"),
                ArrowHead::Triangle,
            ),
            (
                "arrow-head-dart",
                Icon::HeadDart,
                tr!("尖尾", "Dart"),
                ArrowHead::Dart,
            ),
        ]
        .into_iter()
        .map(|(id, icon, title, value)| {
            let why = (ends == ArrowEnds::None)
                .then_some(tr!("先选单箭头或双箭头", "Choose single or double first"));
            (
                id,
                icon,
                title,
                ends != ArrowEnds::None && head == value,
                why,
                boxed(Box::new(move |s: &mut Style| value.apply(s))),
            )
        })
        .collect();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(items::ITEM_GAP))
            .child(self.segment_row(end_segments, cx))
            .child(self.segment_row(head_segments, cx))
            .into_any_element()
    }

    fn line_style(&self, cx: &mut Context<Self>) -> AnyElement {
        let style = self.editor.style;
        let dart = self.editor.tool == Tool::Arrow && arrow_geometry::is_dart(style)
            || self
                .editor
                .selected_annotation()
                .is_some_and(|a| a.tool == Tool::Arrow && arrow_geometry::is_dart(a.style));
        let why = dart.then_some(tr!("尖尾箭头不支持虚线", "Dart arrows can't be dashed"));
        let dashed = style.dashed && !dart;
        let set = |value: bool| {
            Box::new(
                move |v: &mut CaptureView, _: &mut gpui::Window, _: &mut Context<CaptureView>| {
                    let mut s = v.editor.style;
                    s.dashed = value;
                    if s != v.editor.style {
                        v.editor.set_style(s);
                    }
                },
            )
                as Box<dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>)>
        };
        self.segment_row(
            vec![
                (
                    "line-solid",
                    Icon::LineSolid,
                    tr!("实线", "Solid"),
                    !dashed,
                    why,
                    set(false),
                ),
                (
                    "dashed",
                    Icon::LineDash,
                    tr!("虚线", "Dashed"),
                    dashed,
                    why,
                    set(true),
                ),
            ],
            cx,
        )
    }

    fn text_split(&self, outline: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let t = self.editor.style.text;
        let (on, main_id, caret_id, group, icon, title, panel) = if outline {
            (
                t.outline,
                "text-outline",
                "text-outline-menu",
                "split-text-outline",
                Icon::TextOutline,
                tr!("描边", "Outline"),
                ToolbarPanel::TextOutlineStyle,
            )
        } else {
            (
                t.background,
                "text-background",
                "text-background-menu",
                "split-text-background",
                Icon::TextBackground,
                tr!("底色", "Background"),
                ToolbarPanel::TextBackgroundStyle,
            )
        };
        let main = w::split_main(main_id, icon, on, p)
            .tooltip(w::tip(title, "", ""))
            .on_click(self.style_listener(
                move |s| {
                    if outline {
                        s.text.outline = !s.text.outline;
                    } else {
                        s.text.background = !s.text.background;
                    }
                },
                cx,
            ))
            .child(self.probe(main_id));
        let caret = w::split_caret(caret_id, on, p)
            .tooltip(w::tip(
                if outline {
                    tr!("描边设置", "Outline settings")
                } else {
                    tr!("底色设置", "Background settings")
                },
                "",
                "",
            ))
            .on_click(cx.listener(move |v, _, w, cx| v.perform(Action::Panel(panel), w, cx)))
            .child(self.probe(caret_id));
        w::split_toggle(main, caret, group, on, self.toolbar_panel == panel, p).into_any_element()
    }

    /// 单个控件；“样式”弹层也用它绘制展开形态。
    pub(crate) fn property_item(
        &self,
        item: Item,
        ctx: &PropertyContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let style = self.editor.style;
        let tool = ctx.tool;
        match item {
            Item::Count(count) => div()
                .h(px(22.))
                .px(px(9.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .rounded(px(11.))
                .bg(rgb(p.track))
                .whitespace_nowrap()
                .child(trf!("{count} 个对象", "{count} objects"))
                .into_any_element(),
            Item::WatermarkInput(wide) => {
                let mut input = div()
                    .id("watermark-field")
                    .relative()
                    .w(px(if wide { 168. } else { 120. }))
                    .h(px(28.))
                    .flex_shrink_0()
                    .overflow_hidden();
                if let Some(field) = self.watermark_field.as_ref() {
                    input = input.child(field.clone());
                }
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(2.))
                    .child(input.child(self.probe("watermark-field")))
                    .child(
                        w::icon_button("watermark-clear", Icon::Close, State::default(), p)
                            .tooltip(w::tip(tr!("清空水印", "Clear watermark"), "", ""))
                            .on_click(cx.listener(|v, _, w, cx| v.clear_watermark_field(w, cx)))
                            .child(self.probe("watermark-clear")),
                    )
                    .into_any_element()
            }
            Item::Swatches => self.swatch_row(cx),
            Item::ColorButton(with_opacity) => {
                let opacity = (with_opacity && style.opacity < 0.995)
                    .then(|| format!("{:.0}%", style.opacity * 100.));
                w::color_button(
                    "color-menu",
                    style.color,
                    opacity,
                    self.toolbar_panel == ToolbarPanel::Colors,
                    p,
                )
                .tooltip(w::tip(
                    tr!("颜色", "Color"),
                    "",
                    if with_opacity {
                        tr!("含不透明度", "Includes opacity")
                    } else {
                        ""
                    },
                ))
                .on_click(
                    cx.listener(|v, _, w, cx| {
                        v.perform(Action::Panel(ToolbarPanel::Colors), w, cx)
                    }),
                )
                .child(self.probe("color-menu"))
                .into_any_element()
            }
            Item::Width => self.value_item(
                "width-menu",
                Lead::Dot(style.width),
                value_label(style.width),
                item.value_text_width(),
                ToolbarPanel::Width,
                tr!("粗细", "Width"),
                cx,
            ),
            Item::SpotBorder => self.value_item(
                "width-menu",
                Lead::Dot(style.width.max(2.)),
                value_label(style.width),
                item.value_text_width(),
                ToolbarPanel::Width,
                tr!("边框", "Border"),
                cx,
            ),
            Item::BrushSize => self.value_item(
                "width-menu",
                Lead::Dot(style.width / 3.),
                value_label(style.width),
                item.value_text_width(),
                ToolbarPanel::Width,
                match tool {
                    Tool::LocalFill => tr!("大小", "Size"),
                    Tool::PixelEraser | Tool::Eraser => tr!("直径", "Diameter"),
                    _ => tr!("笔宽", "Brush"),
                },
                cx,
            ),
            Item::Font => self.value_item(
                "font-menu",
                Lead::Icon(Icon::FontSize),
                value_label(style.font_size),
                item.value_text_width(),
                ToolbarPanel::Font,
                tr!("字号", "Font size"),
                cx,
            ),
            Item::Opacity => self.value_item(
                "opacity-menu",
                Lead::Icon(Icon::Opacity),
                format!("{:.0}%", style.opacity * 100.),
                item.value_text_width(),
                ToolbarPanel::Opacity,
                tr!("不透明度", "Opacity"),
                cx,
            ),
            Item::Dim => self.value_item(
                "opacity-menu",
                Lead::Icon(Icon::Spotlight),
                format!("{:.0}%", style.opacity * 100.),
                item.value_text_width(),
                ToolbarPanel::Opacity,
                tr!("暗化", "Dim"),
                cx,
            ),
            Item::Strength => {
                // 智能打码的强度取自它记住的效果样式。
                let (block, blur) = if tool == Tool::SmartMosaic {
                    let (effect, style) = self.editor.effect_context();
                    (style.block_size, effect == Tool::Blur)
                } else {
                    (
                        style.block_size,
                        matches!(tool, Tool::Blur | Tool::BlurBrush),
                    )
                };
                self.value_item(
                    "block-menu",
                    Lead::Icon(if blur { Icon::Blur } else { Icon::Mosaic }),
                    block.clamp(2, 64).to_string(),
                    item.value_text_width(),
                    ToolbarPanel::Block,
                    if blur {
                        tr!("模糊强度", "Blur strength")
                    } else {
                        tr!("色块大小", "Block size")
                    },
                    cx,
                )
            }
            Item::Radius => {
                let selection = self.editor.radius_selection();
                let text = if self
                    .editor
                    .rectangle_corner_selection()
                    .is_some_and(|s| s.has_independent())
                {
                    items::radius_independent_label().to_owned()
                } else if selection.mixed {
                    items::radius_mixed_label().to_owned()
                } else {
                    value_label(selection.value)
                };
                self.value_item(
                    "radius-menu",
                    Lead::Icon(Icon::Radius),
                    text,
                    item.value_text_width(),
                    ToolbarPanel::Radius,
                    tr!("圆角", "Radius"),
                    cx,
                )
            }
            Item::Angle | Item::WatermarkAngle => {
                let angle = if item == Item::WatermarkAngle {
                    self.editor.watermark_rotation().unwrap_or(0.)
                } else {
                    self.editor.selected_annotation().map_or(0., |a| a.rotation)
                };
                self.value_item(
                    "angle-menu",
                    Lead::Icon(Icon::RotateRight),
                    format!("{angle:.0}°"),
                    item.value_text_width(),
                    ToolbarPanel::Angle,
                    if item == Item::WatermarkAngle {
                        tr!("水印角度", "Watermark angle")
                    } else {
                        tr!("角度", "Angle")
                    },
                    cx,
                )
            }
            Item::FillMode => self.style_segments(
                vec![
                    (
                        "fill-stroke",
                        Icon::FillStroke,
                        tr!("描边", "Outline"),
                        !style.filled,
                        |s| s.filled = false,
                    ),
                    (
                        "filled",
                        Icon::FillSolid,
                        tr!("填充", "Fill"),
                        style.filled,
                        |s| s.filled = true,
                    ),
                ],
                cx,
            ),
            Item::FillMenu => self.dropdown_item(
                "fill-menu",
                if style.filled {
                    Icon::FillSolid
                } else {
                    Icon::FillStroke
                },
                ToolbarPanel::Choice(ChoiceKind::Fill),
                State::default(),
                (tr!("填充方式", "Fill"), String::new()),
                cx,
            ),
            Item::LineStyle => self.line_style(cx),
            Item::LineStyleMenu => {
                let dart = tool == Tool::Arrow && arrow_geometry::is_dart(style);
                self.dropdown_item(
                    "line-style-menu",
                    if style.dashed && !dart {
                        Icon::LineDash
                    } else {
                        Icon::LineSolid
                    },
                    ToolbarPanel::Choice(ChoiceKind::LineStyle),
                    State::default().disabled(dart),
                    (
                        tr!("线型", "Line"),
                        if dart {
                            tr!("尖尾箭头不支持虚线", "Dart arrows can't be dashed").to_owned()
                        } else {
                            String::new()
                        },
                    ),
                    cx,
                )
            }
            Item::ArrowEnds => self.arrow_ends(cx),
            Item::ArrowMenu => {
                let preset = crate::arrow_geometry::ArrowPreset::of(style);
                w::dropdown_with(
                    "arrow-menu",
                    crate::capture_arrow_properties::arrow_preview(style, 30., 16., p.foreground),
                    State::open(self.toolbar_panel == ToolbarPanel::Arrow),
                    p,
                )
                .tooltip(w::tip(tr!("箭头", "Arrow"), "", preset.label()))
                .on_click(
                    cx.listener(|v, _, w, cx| v.perform(Action::Panel(ToolbarPanel::Arrow), w, cx)),
                )
                .child(self.probe("arrow-menu"))
                .into_any_element()
            }
            Item::Straighten => self.icon_item(
                "straighten-arrow",
                Icon::Line,
                State::default(),
                (
                    tr!("拉直", "Straighten"),
                    "",
                    tr!("曲线箭头改回直线", "Turn the curved arrow straight"),
                ),
                Action::StraightenArrow,
                cx,
            ),
            Item::PolylineSmooth => self.toggle_item(
                "polyline-smooth",
                Icon::Smooth,
                style.polyline_smooth,
                (tr!("平滑曲线", "Smooth curve"), ""),
                |s| s.polyline_smooth = !s.polyline_smooth,
                cx,
            ),
            Item::PolylineClosed => self.toggle_item(
                "filled",
                Icon::ClosedFill,
                style.filled,
                (tr!("闭合填充", "Closed fill"), ""),
                |s| s.filled = !s.filled,
                cx,
            ),
            Item::DeleteNode => {
                let enabled = self.editor.active_node().is_some();
                self.icon_item(
                    "delete-node",
                    Icon::DeleteVertex,
                    State::default().disabled(!enabled),
                    (
                        tr!("删除顶点", "Delete point"),
                        "Del",
                        if enabled {
                            ""
                        } else {
                            tr!(
                                "先在折线上点选一个顶点",
                                "Pick a point on the polyline first"
                            )
                        },
                    ),
                    Action::DeleteNode,
                    cx,
                )
            }
            Item::PenSmooth => self.toggle_item(
                "pencil-smooth",
                Icon::Smooth,
                style.pencil_smooth,
                (
                    tr!("保角平滑", "Keep corners"),
                    if style.pencil_smooth {
                        ""
                    } else {
                        tr!("当前为原始笔迹", "Currently raw stroke")
                    },
                ),
                |s| s.pencil_smooth = !s.pencil_smooth,
                cx,
            ),
            Item::Blend => {
                let set = |blend: HighlighterBlend| {
                    Box::new(
                        move |v: &mut CaptureView,
                              _: &mut gpui::Window,
                              _: &mut Context<CaptureView>| {
                            let mut s = v.editor.style;
                            if s.highlighter_blend == blend {
                                return;
                            }
                            s.highlighter_blend = blend;
                            s.opacity = match blend {
                                HighlighterBlend::Alpha => 96. / 255.,
                                HighlighterBlend::Multiply => 1.,
                            };
                            v.editor.set_style(s);
                        },
                    )
                        as Box<
                            dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>),
                        >
                };
                self.segment_row(
                    vec![
                        (
                            "highlighter-blend-multiply",
                            Icon::BlendMultiply,
                            tr!("正片叠底", "Multiply"),
                            style.highlighter_blend == HighlighterBlend::Multiply,
                            None,
                            set(HighlighterBlend::Multiply),
                        ),
                        (
                            "highlighter-blend-alpha",
                            Icon::BlendAlpha,
                            tr!("透明叠加", "Overlay"),
                            style.highlighter_blend == HighlighterBlend::Alpha,
                            None,
                            set(HighlighterBlend::Alpha),
                        ),
                    ],
                    cx,
                )
            }
            Item::BlendMenu => self.dropdown_item(
                "blend-menu",
                if style.highlighter_blend == HighlighterBlend::Alpha {
                    Icon::BlendAlpha
                } else {
                    Icon::BlendMultiply
                },
                ToolbarPanel::Choice(ChoiceKind::Blend),
                State::default(),
                (tr!("叠加方式", "Blend"), String::new()),
                cx,
            ),
            Item::Stroke => {
                let shape = self.editor.highlighter_shape();
                let set = |shape: HighlighterShape| {
                    Box::new(
                        move |v: &mut CaptureView,
                              _: &mut gpui::Window,
                              _: &mut Context<CaptureView>| {
                            v.editor.set_highlighter_shape(shape)
                        },
                    )
                        as Box<
                            dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>),
                        >
                };
                self.segment_row(
                    vec![
                        (
                            "highlighter-shape-freehand",
                            Icon::Smooth,
                            tr!("手绘", "Freehand"),
                            shape == HighlighterShape::Freehand,
                            None,
                            set(HighlighterShape::Freehand),
                        ),
                        (
                            "highlighter-shape-line",
                            Icon::Line,
                            tr!("拉直", "Straight"),
                            shape == HighlighterShape::Line,
                            None,
                            set(HighlighterShape::Line),
                        ),
                    ],
                    cx,
                )
            }
            Item::StrokeMenu => self.dropdown_item(
                "stroke-menu",
                if self.editor.highlighter_shape() == HighlighterShape::Line {
                    Icon::Line
                } else {
                    Icon::Smooth
                },
                ToolbarPanel::Choice(ChoiceKind::Stroke),
                State::default(),
                (tr!("笔迹", "Stroke"), String::new()),
                cx,
            ),
            Item::FontFamily => {
                w::dropdown_text(
                    "font-family-menu",
                    style.text.font.name(),
                    State::open(self.toolbar_panel == ToolbarPanel::FontFamily),
                    p,
                )
                .tooltip(w::tip(tr!("字体", "Font"), "", ""))
                .on_click(cx.listener(|v, _, w, cx| {
                    v.perform(Action::Panel(ToolbarPanel::FontFamily), w, cx)
                }))
                .child(self.probe("font-family-menu"))
                .into_any_element()
            }
            Item::Glyphs | Item::BoldOnly => {
                let t = style.text;
                let mut segments: Vec<(&'static str, Icon, &'static str, bool, fn(&mut Style))> =
                    vec![("bold", Icon::Bold, tr!("粗体", "Bold"), style.bold, |s| {
                        s.bold = !s.bold
                    })];
                if item == Item::Glyphs {
                    segments.extend([
                        (
                            "italic",
                            Icon::Italic,
                            tr!("斜体", "Italic"),
                            t.italic,
                            (|s: &mut Style| s.text.italic = !s.text.italic) as fn(&mut Style),
                        ),
                        (
                            "underline",
                            Icon::Underline,
                            tr!("下划线", "Underline"),
                            t.underline,
                            |s| s.text.underline = !s.text.underline,
                        ),
                        (
                            "strike",
                            Icon::Strike,
                            tr!("删除线", "Strikethrough"),
                            t.strike,
                            |s| s.text.strike = !s.text.strike,
                        ),
                    ]);
                }
                self.style_segments(segments, cx)
            }
            Item::TextStyleMenu => self.dropdown_item(
                "text-format-menu",
                Icon::TextStyle,
                ToolbarPanel::TextFormat,
                State::default(),
                (
                    tr!("文字样式", "Text style"),
                    tr!(
                        "斜体、下划线、删除线、对齐",
                        "Italic, underline, strike, align"
                    )
                    .to_owned(),
                ),
                cx,
            ),
            Item::Align => self.align_segments(cx),
            Item::TextOutline => self.text_split(true, cx),
            Item::TextBackground => self.text_split(false, cx),
            Item::SerialBold => self.toggle_item(
                "bold",
                Icon::Bold,
                style.bold,
                (tr!("粗体", "Bold"), ""),
                |s| s.bold = !s.bold,
                cx,
            ),
            Item::Effect => self.redaction_effect(ctx, cx),
            Item::Region => self.redaction_region(ctx, cx),
            Item::SmartEffect => {
                let (effect, _) = self.editor.effect_context();
                let set = |next: Tool| {
                    Box::new(
                        move |v: &mut CaptureView,
                              w: &mut gpui::Window,
                              cx: &mut Context<CaptureView>| {
                            v.perform(Action::SmartEffect(next), w, cx)
                        },
                    )
                        as Box<
                            dyn Fn(&mut CaptureView, &mut gpui::Window, &mut Context<CaptureView>),
                        >
                };
                self.segment_row(
                    vec![
                        (
                            "smart-mosaic-effect",
                            Icon::Mosaic,
                            tr!("马赛克", "Mosaic"),
                            effect == Tool::Mosaic,
                            None,
                            set(Tool::Mosaic),
                        ),
                        (
                            "smart-blur-effect",
                            Icon::Blur,
                            tr!("模糊", "Blur"),
                            effect == Tool::Blur,
                            None,
                            set(Tool::Blur),
                        ),
                    ],
                    cx,
                )
            }
            Item::Smart => self.icon_item(
                "smart-effect",
                Icon::Sparkle,
                State::default(),
                (
                    tr!("智能打码", "Smart redact"),
                    "",
                    tr!(
                        "自动找出文字区域，按当前效果处理",
                        "Finds text regions and applies the current effect"
                    ),
                ),
                Action::SmartMosaic,
                cx,
            ),
            Item::SmartDone => {
                w::text_button("smart-leave", tr!("完成选择", "Done"), false, true, p)
                    .tooltip(w::tip(tr!("完成选择", "Done"), "Enter", ""))
                    .on_click(cx.listener(|v, _, w, cx| v.perform(Action::Tool(Tool::Edit), w, cx)))
                    .child(self.probe("smart-leave"))
                    .into_any_element()
            }
            Item::LassoApply => {
                let enabled = self.editor.lasso_draft.is_some();
                w::text_button(
                    "apply-lasso",
                    tr!("应用 · Enter", "Apply · Enter"),
                    true,
                    enabled,
                    p,
                )
                .on_click(cx.listener(|v, _, w, cx| {
                    if v.editor.lasso_draft.is_some() {
                        v.perform(Action::ApplyLasso, w, cx)
                    }
                }))
                .child(self.probe("apply-lasso"))
                .into_any_element()
            }
            Item::LassoClear => w::text_button(
                "clear-lasso",
                tr!("清除套索", "Clear lasso"),
                false,
                true,
                p,
            )
            .on_click(cx.listener(|v, _, w, cx| v.perform(Action::ClearLasso, w, cx)))
            .child(self.probe("clear-lasso"))
            .into_any_element(),
            Item::Hint(hint) => {
                let status = matches!(hint, Hint::RadiusApplies(..) | Hint::RadiusNone(_));
                w::hint(hint.text(), p)
                    .relative()
                    .when(status, |d| d.child(self.probe("radius-selection-status")))
                    .into_any_element()
            }
            Item::StyleMenu => self.dropdown_item(
                "tool-style-menu",
                Icon::Sliders,
                ToolbarPanel::ToolStyle,
                State::default(),
                (tr!("样式", "Style"), String::new()),
                cx,
            ),
            Item::EditText => self.icon_item(
                "edit-text",
                Icon::Pen,
                State::default(),
                (
                    tr!("编辑文字", "Edit text"),
                    tr!("双击", "Double-click"),
                    "",
                ),
                Action::EditText,
                cx,
            ),
            Item::Arrange => {
                let depth = self.editor.group_scope_depth();
                self.dropdown_item(
                    "arrange-menu",
                    Icon::Layers,
                    ToolbarPanel::Arrange,
                    State::default(),
                    (
                        tr!("排列与组合", "Arrange & group"),
                        if depth > 0 {
                            trf!("组合 · 第 {depth} 层", "Group · level {depth}")
                        } else {
                            String::new()
                        },
                    ),
                    cx,
                )
            }
            Item::Delete => self.icon_item(
                "delete-object",
                Icon::Trash,
                State {
                    danger: true,
                    ..State::default()
                },
                (tr!("删除", "Delete"), "Del", ""),
                Action::Delete,
                cx,
            ),
        }
    }

    pub(crate) fn align_segments(&self, cx: &mut Context<Self>) -> AnyElement {
        let alignment = self.editor.style.text.alignment;
        self.style_segments(
            vec![
                (
                    "align-left",
                    Icon::AlignLeft,
                    tr!("左对齐", "Align left"),
                    alignment == Alignment::Left,
                    |s| s.text.alignment = Alignment::Left,
                ),
                (
                    "align-center",
                    Icon::AlignCenter,
                    tr!("居中", "Center"),
                    alignment == Alignment::Center,
                    |s| s.text.alignment = Alignment::Center,
                ),
                (
                    "align-right",
                    Icon::AlignRight,
                    tr!("右对齐", "Align right"),
                    alignment == Alignment::Right,
                    |s| s.text.alignment = Alignment::Right,
                ),
            ],
            cx,
        )
    }

    /// 斜体、下划线、删除线（多选分段），给“文字样式”弹层用。
    pub(crate) fn glyph_segments(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.editor.style.text;
        self.style_segments(
            vec![
                (
                    "italic",
                    Icon::Italic,
                    tr!("斜体", "Italic"),
                    t.italic,
                    |s| s.text.italic = !s.text.italic,
                ),
                (
                    "underline",
                    Icon::Underline,
                    tr!("下划线", "Underline"),
                    t.underline,
                    |s| s.text.underline = !s.text.underline,
                ),
                (
                    "strike",
                    Icon::Strike,
                    tr!("删除线", "Strikethrough"),
                    t.strike,
                    |s| s.text.strike = !s.text.strike,
                ),
            ],
            cx,
        )
    }

    pub(crate) fn property_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        selection_actions: Option<crate::selection_action_layout::SelectionActionLayout>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = self.toolbar_panel;
        if let ToolbarPanel::CustomColor(target) = panel {
            return self.color_picker_popup(target, layout, anchor, selection_actions, cx);
        }
        match panel {
            ToolbarPanel::FontFamily => {
                return self
                    .font_family_popup(layout, anchor, cx)
                    .into_any_element()
            }
            ToolbarPanel::TextFormat => {
                return self
                    .text_format_popup(layout, anchor, cx)
                    .into_any_element()
            }
            ToolbarPanel::TextOutlineStyle | ToolbarPanel::TextBackgroundStyle => {
                return self
                    .text_detail_popup(panel == ToolbarPanel::TextOutlineStyle, layout, anchor, cx)
                    .into_any_element()
            }
            ToolbarPanel::Arrow => return self.arrow_popup(layout, anchor, cx).into_any_element(),
            ToolbarPanel::Angle => return self.angle_popup(layout, anchor, cx),
            ToolbarPanel::Arrange => return self.arrange_popup(layout, anchor, cx),
            ToolbarPanel::Colors => return self.colors_popup(layout, anchor, cx),
            ToolbarPanel::Choice(kind) => return self.choice_popup(kind, layout, anchor, cx),
            ToolbarPanel::ToolStyle => return self.tool_style_popup(layout, anchor, cx),
            _ => {}
        }
        if let Some(spec) = crate::parameter_control::ParameterSpec::for_view(panel, self) {
            if let Some((width, height)) =
                crate::toolbar_layout::parameter_popup_size(panel, spec.tool)
            {
                return self
                    .panel("toolbar-popup", layout.popup(width, height, anchor, true))
                    .rounded(px(10.))
                    .p(px(12.))
                    .child(crate::parameter_control::popup(
                        cx.entity().downgrade(),
                        spec,
                        Palette::new(self.light_theme),
                        self.ui_positions.clone(),
                        self.wheel_settings,
                    ))
                    .child(self.probe("toolbar-popup"))
                    .into_any_element();
            }
        }
        let message = if panel == ToolbarPanel::Radius {
            tr!(
                "当前选择没有支持圆角的对象",
                "No object in the selection supports corner radius"
            )
        } else {
            return div().into_any_element();
        };
        let rect = layout.popup(300., 96., anchor, true);
        let p = Palette::new(self.light_theme);
        self.panel("toolbar-popup", rect)
            .rounded(px(10.))
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(message)
            .child(
                div().flex().justify_end().child(
                    w::text_button(
                        "radius-unavailable-close",
                        tr!("关闭", "Close"),
                        false,
                        true,
                        p,
                    )
                    .on_click(cx.listener(|v, _, w, cx| {
                        v.perform(Action::Panel(ToolbarPanel::None), w, cx)
                    })),
                ),
            )
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }
}
