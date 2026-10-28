//! 每工具的跨会话样式预设；只保存参数，不保存标注、文字正文或桌面内容。
//! 编辑期间 update 仅更新内存，调用方在会话关闭或输出时显式 save 一次。
use crate::{
    annotation::{HighlighterBlend, HighlighterShape, Style, Tool},
    arrow_geometry::ArrowHead,
    config_json::JsonFile,
    text_format::{Alignment, FontFamily},
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const SCHEMA_VERSION: u32 = 1;
const TOOLS: [(Tool, &str); 21] = [
    (Tool::Rectangle, "rectangle"),
    (Tool::Ellipse, "ellipse"),
    (Tool::Triangle, "triangle"),
    (Tool::Diamond, "diamond"),
    (Tool::Star, "star"),
    (Tool::Arrow, "arrow"),
    (Tool::Pen, "pen"),
    (Tool::Mosaic, "mosaic"),
    (Tool::Highlighter, "highlighter"),
    (Tool::HighlighterRect, "highlighterRect"),
    (Tool::Polyline, "polyline"),
    (Tool::Text, "text"),
    (Tool::Watermark, "watermark"),
    (Tool::Serial, "serial"),
    (Tool::Blur, "blur"),
    (Tool::MosaicBrush, "mosaicBrush"),
    (Tool::BlurBrush, "blurBrush"),
    (Tool::LocalFill, "localFill"),
    (Tool::Spotlight, "spotlight"),
    (Tool::Eraser, "eraser"),
    (Tool::PixelEraser, "pixelEraser"),
];

pub fn persistent_tool(tool: Tool) -> bool {
    TOOLS.iter().any(|(t, _)| *t == tool)
}

/// 与编辑器原有工具隔离默认值相同；选区、编辑和套索无可保存样式。
pub fn default_style(tool: Tool) -> Style {
    let mut style = Style::default();
    if matches!(
        tool,
        Tool::Highlighter | Tool::HighlighterRect | Tool::Watermark
    ) {
        style.opacity = 0.35;
        style.width = 24.;
        if tool != Tool::Watermark {
            style.color = 0xffd43b;
            style.highlighter_blend = HighlighterBlend::Multiply;
            style.opacity = 1.;
        }
    } else if tool == Tool::LocalFill {
        style.width = 10.;
    } else if tool == Tool::PixelEraser {
        style.width = 16.;
    } else if tool == Tool::Spotlight {
        style.color = 0xffffff;
        style.width = 2.;
        style.opacity = 0.5;
    } else if (tool.freehand() && tool.effect()) || tool == Tool::Eraser {
        style.width = 48.;
    }
    style
}

/// 已存在的高亮节采用旧渲染默认值，显式新字段再覆盖，避免升级改变旧图片效果。
fn existing_section_style(tool: Tool) -> Style {
    let mut style = default_style(tool);
    if matches!(tool, Tool::Highlighter | Tool::HighlighterRect) {
        style.highlighter_blend = HighlighterBlend::Alpha;
        style.opacity = 0.35;
    }
    style
}

#[derive(Clone, Default)]
pub struct ToolPresets {
    styles: HashMap<Tool, Style>,
    baseline: HashMap<Tool, Style>,
    dirty: HashSet<Tool>,
}
impl ToolPresets {
    pub fn load() -> Result<Self> {
        Self::load_at(presets_path())
    }
    /// 缺失或损坏的配置回退默认（损坏文件由 config_recovery 备份）；其他读取错误交由调用方报告。
    pub fn load_at(path: impl AsRef<Path>) -> Result<Self> {
        Ok(json_file(path.as_ref())
            .load()?
            .map(Self::from_file)
            .unwrap_or_default())
    }
    pub fn style(&self, tool: Tool) -> Style {
        self.styles
            .get(&tool)
            .copied()
            .unwrap_or_else(|| default_style(tool))
    }
    pub fn styles(&self) -> impl Iterator<Item = (Tool, Style)> + '_ {
        TOOLS.iter().map(|(tool, _)| (*tool, self.style(*tool)))
    }
    /// 逐项舍弃无效值，保留该项原有效值；不会在每次滚轮或颜色点击时写磁盘。
    pub fn update(&mut self, tool: Tool, style: Style) -> bool {
        if !persistent_tool(tool) {
            return false;
        }
        let before = self.style(tool);
        let style = validated(tool, style, before);
        if style == before {
            return false;
        }
        if style == default_style(tool) {
            self.styles.remove(&tool);
        } else {
            self.styles.insert(tool, style);
        }
        if self.styles.get(&tool) == self.baseline.get(&tool) {
            self.dirty.remove(&tool);
        } else {
            self.dirty.insert(tool);
        }
        true
    }
    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }
    pub fn save(&mut self) -> Result<bool> {
        self.save_at(presets_path())
    }
    /// 原子替换，并只合并本会话改过的工具，避免贴图和截图会话覆盖彼此预设。
    /// 非阻塞跨进程锁忙时返回错误，调用方仍可完成当前图片输出。
    pub fn save_at(&mut self, path: impl AsRef<Path>) -> Result<bool> {
        if !self.is_dirty() {
            return Ok(false);
        }
        let json = json_file(path.as_ref());
        let _lock = json.lock()?;
        let mut merged = json.read_locked()?.map(Self::from_file).unwrap_or_default();
        let before = merged.styles.clone();
        for tool in &self.dirty {
            if let Some(style) = self.styles.get(tool) {
                merged.styles.insert(*tool, *style);
            } else {
                merged.styles.remove(tool);
            }
        }
        let changed = before != merged.styles;
        if changed {
            json.write_locked(&merged.file())?;
        }
        // 合并只影响磁盘。本会话 Editor 仍持有自己的缓存，回灌其他窗口的值
        // 会使下一次输出/关闭把未编辑工具误判为修改并覆盖回旧值。
        self.baseline = self.styles.clone();
        self.dirty.clear();
        Ok(changed)
    }
    fn from_file(file: PresetsFile) -> Self {
        let mut presets = Self::default();
        for (id, value) in file.tools {
            let (Some((tool, _)), Value::Object(map)) =
                (TOOLS.iter().find(|(_, name)| *name == id), value)
            else {
                continue;
            };
            presets.styles.insert(*tool, style_from(*tool, map));
        }
        presets
            .styles
            .retain(|tool, style| *style != default_style(*tool));
        presets.baseline = presets.styles.clone();
        presets
    }
    fn file(&self) -> PresetsFile {
        let mut tools = Map::new();
        for (tool, id) in TOOLS {
            if let Some(style) = self.styles.get(&tool) {
                if let Ok(value) = serde_json::to_value(PresetFields::new(tool, style)) {
                    tools.insert(id.into(), value);
                }
            }
        }
        PresetsFile { tools }
    }
    /// 旧 tool-presets.ini 解析器，只用于迁移；下个版本删除。
    fn parse_legacy_ini(text: &str) -> Self {
        let mut presets = Self::default();
        let mut tool = None;
        for line in text.lines() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with([';', '#']) {
                continue;
            }
            if line.starts_with('[') {
                tool = line
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .and_then(|id| TOOLS.iter().find(|(_, name)| *name == id))
                    .map(|(tool, _)| *tool);
                if let Some(tool) =
                    tool.filter(|tool| matches!(tool, Tool::Highlighter | Tool::HighlighterRect))
                {
                    presets
                        .styles
                        .entry(tool)
                        .or_insert_with(|| existing_section_style(tool));
                }
                continue;
            }
            let (Some(tool), Some((key, value))) = (tool, line.split_once('=')) else {
                continue;
            };
            let style = presets
                .styles
                .entry(tool)
                .or_insert_with(|| existing_section_style(tool));
            parse_field(tool, style, key.trim(), value.trim());
        }
        presets
            .styles
            .retain(|tool, style| *style != default_style(*tool));
        presets.baseline = presets.styles.clone();
        presets
    }
    #[cfg(test)]
    fn encode_legacy_ini(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::from("version=1\n");
        for (tool, id) in TOOLS {
            let Some(s) = self.styles.get(&tool) else {
                continue;
            };
            writeln!(out, "\n[{id}]").expect("writing to String cannot fail");
            macro_rules! field {
                ($wire:literal, $value:expr) => {
                    writeln!(out, concat!($wire, "={}"), $value)
                        .expect("writing to String cannot fail")
                };
            }
            field!("color", format_args!("{:06x}", s.color));
            field!("width", s.width);
            field!("blockSize", s.block_size);
            field!("opacity", s.opacity);
            if matches!(tool, Tool::Highlighter | Tool::HighlighterRect) {
                field!(
                    "highlighterBlend",
                    match s.highlighter_blend {
                        HighlighterBlend::Alpha => "alpha",
                        HighlighterBlend::Multiply => "multiply",
                    }
                );
            }
            if tool == Tool::Highlighter {
                field!(
                    "highlighterShape",
                    match s.highlighter_shape {
                        HighlighterShape::Freehand => "freehand",
                        HighlighterShape::Line => "line",
                    }
                );
            }
            field!("filled", s.filled);
            field!("radius", s.radius);
            if tool == Tool::Rectangle {
                if let Some(r) = s.rectangle_radii {
                    field!(
                        "rectangleRadii",
                        format_args!("{},{},{},{}", r[0], r[1], r[2], r[3])
                    );
                }
            }
            if matches!(tool, Tool::Mosaic | Tool::Blur) {
                field!("effectRadius", s.effect_radius);
            }
            field!("fontSize", s.font_size);
            field!("bold", s.bold);
            field!("fontFamily", s.text.font.name());
            field!("italic", s.text.italic);
            field!("underline", s.text.underline);
            field!("strike", s.text.strike);
            field!("textAlignment", alignment_name(s.text.alignment));
            field!("textOutline", s.text.outline);
            field!(
                "textOutlineColor",
                format_args!("{:06x}", s.text.outline_color)
            );
            field!("textOutlineWidth", s.text.outline_width);
            field!("textBackground", s.text.background);
            field!(
                "textBackgroundColor",
                format_args!("{:06x}", s.text.background_color)
            );
            field!("textBackgroundOpacity", s.text.background_opacity);
            field!("textBackgroundRadius", s.text.background_radius);
            field!("arrowHead", arrow_name(s.arrow_head));
            field!("arrowStart", s.arrow_start);
            field!("arrowEnd", s.arrow_end);
            field!("dashed", s.dashed);
            field!("pencilSmooth", s.pencil_smooth);
            field!("polylineSmooth", s.polyline_smooth);
        }
        out
    }
}
fn presets_path() -> PathBuf {
    crate::preferences::data_dir().join("tool-presets.json")
}
fn json_file(path: &Path) -> JsonFile<'_, PresetsFile> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: |text| ToolPresets::parse_legacy_ini(text).file(),
    }
}

/// tool-presets.json：`tools` 下每个工具一组字段，只存与默认不同的工具。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct PresetsFile {
    tools: Map<String, Value>,
}
/// 一个工具的样式字段；颜色是 6 位十六进制 RRGGBB。只对部分工具有意义的字段
/// （高亮混合／形状、矩形四角、效果圆角）仅在这些工具上出现。
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct PresetFields {
    color: String,
    width: f32,
    block_size: u32,
    opacity: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlighter_blend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlighter_shape: Option<String>,
    filled: bool,
    radius: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    rectangle_radii: Option<[f32; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effect_radius: Option<f32>,
    font_size: f32,
    bold: bool,
    font_family: String,
    italic: bool,
    underline: bool,
    strike: bool,
    text_alignment: String,
    text_outline: bool,
    text_outline_color: String,
    text_outline_width: f32,
    text_background: bool,
    text_background_color: String,
    text_background_opacity: f32,
    text_background_radius: f32,
    arrow_head: String,
    arrow_start: bool,
    arrow_end: bool,
    dashed: bool,
    pencil_smooth: bool,
    polyline_smooth: bool,
}
impl Default for PresetFields {
    fn default() -> Self {
        Self::new(Tool::Pen, &default_style(Tool::Pen))
    }
}
impl PresetFields {
    fn new(tool: Tool, s: &Style) -> Self {
        let highlighter = matches!(tool, Tool::Highlighter | Tool::HighlighterRect);
        Self {
            color: format!("{:06x}", s.color),
            width: s.width,
            block_size: s.block_size,
            opacity: s.opacity,
            highlighter_blend: highlighter.then(|| {
                match s.highlighter_blend {
                    HighlighterBlend::Alpha => "alpha",
                    HighlighterBlend::Multiply => "multiply",
                }
                .into()
            }),
            highlighter_shape: (tool == Tool::Highlighter).then(|| {
                match s.highlighter_shape {
                    HighlighterShape::Freehand => "freehand",
                    HighlighterShape::Line => "line",
                }
                .into()
            }),
            filled: s.filled,
            radius: s.radius,
            rectangle_radii: s.rectangle_radii.filter(|_| tool == Tool::Rectangle),
            effect_radius: matches!(tool, Tool::Mosaic | Tool::Blur).then_some(s.effect_radius),
            font_size: s.font_size,
            bold: s.bold,
            font_family: s.text.font.name().into(),
            italic: s.text.italic,
            underline: s.text.underline,
            strike: s.text.strike,
            text_alignment: alignment_name(s.text.alignment).into(),
            text_outline: s.text.outline,
            text_outline_color: format!("{:06x}", s.text.outline_color),
            text_outline_width: s.text.outline_width,
            text_background: s.text.background,
            text_background_color: format!("{:06x}", s.text.background_color),
            text_background_opacity: s.text.background_opacity,
            text_background_radius: s.text.background_radius,
            arrow_head: arrow_name(s.arrow_head).into(),
            arrow_start: s.arrow_start,
            arrow_end: s.arrow_end,
            dashed: s.dashed,
            pencil_smooth: s.pencil_smooth,
            polyline_smooth: s.polyline_smooth,
        }
    }
}
/// 从该工具的旧节默认值起读；与旧 INI 相同，无效字段保留原值，越界数值由 validated 收回。
fn style_from(tool: Tool, map: Map<String, Value>) -> Style {
    let base = existing_section_style(tool);
    let f = crate::config_json::overlay(PresetFields::new(tool, &base), map);
    let mut s = base;
    macro_rules! set {
        ($target:expr, $value:expr) => {
            if let Some(value) = $value {
                $target = value;
            }
        };
    }
    set!(s.color, color(&f.color));
    s.width = f.width;
    s.block_size = f.block_size;
    s.opacity = f.opacity;
    if matches!(tool, Tool::Highlighter | Tool::HighlighterRect) {
        set!(
            s.highlighter_blend,
            f.highlighter_blend.map(|value| match value.as_str() {
                "multiply" => HighlighterBlend::Multiply,
                _ => HighlighterBlend::Alpha,
            })
        );
    }
    if tool == Tool::Highlighter {
        set!(
            s.highlighter_shape,
            f.highlighter_shape.map(|value| match value.as_str() {
                "line" => HighlighterShape::Line,
                _ => HighlighterShape::Freehand,
            })
        );
    }
    s.filled = f.filled;
    s.radius = f.radius;
    if let Some(radii) = f.rectangle_radii.filter(|radii| {
        tool == Tool::Rectangle
            && radii
                .iter()
                .all(|value| value.is_finite() && (0. ..=2048.).contains(value))
    }) {
        s.rectangle_radii = Some(radii);
        s.radius = radii[0];
    }
    set!(s.effect_radius, f.effect_radius);
    s.font_size = f.font_size;
    s.bold = f.bold;
    let font = f.font_family.as_str();
    if !font.is_empty() && font.len() <= 256 && !font.chars().any(char::is_control) {
        s.text.font = if font.eq_ignore_ascii_case("Microsoft YaHei UI") {
            FontFamily::default()
        } else {
            FontFamily::named(font)
        };
    }
    s.text.italic = f.italic;
    s.text.underline = f.underline;
    s.text.strike = f.strike;
    set!(s.text.alignment, alignment_from(&f.text_alignment));
    s.text.outline = f.text_outline;
    set!(s.text.outline_color, color(&f.text_outline_color));
    s.text.outline_width = f.text_outline_width;
    s.text.background = f.text_background;
    set!(s.text.background_color, color(&f.text_background_color));
    s.text.background_opacity = f.text_background_opacity;
    s.text.background_radius = f.text_background_radius;
    set!(s.arrow_head, arrow_from(&f.arrow_head));
    s.arrow_start = f.arrow_start;
    s.arrow_end = f.arrow_end;
    s.dashed = f.dashed;
    s.pencil_smooth = f.pencil_smooth;
    s.polyline_smooth = f.polyline_smooth;
    validated(tool, s, base)
}
fn arrow_from(value: &str) -> Option<ArrowHead> {
    match value {
        "open" => Some(ArrowHead::Open),
        "triangle" => Some(ArrowHead::Triangle),
        "dart" => Some(ArrowHead::Dart),
        _ => None,
    }
}
fn alignment_from(value: &str) -> Option<Alignment> {
    match value {
        "left" => Some(Alignment::Left),
        "center" => Some(Alignment::Center),
        "right" => Some(Alignment::Right),
        _ => None,
    }
}
fn arrow_name(head: ArrowHead) -> &'static str {
    match head {
        ArrowHead::Open => "open",
        ArrowHead::Triangle => "triangle",
        ArrowHead::Dart => "dart",
    }
}
fn alignment_name(alignment: Alignment) -> &'static str {
    match alignment {
        Alignment::Left => "left",
        Alignment::Center => "center",
        Alignment::Right => "right",
    }
}
fn color(value: &str) -> Option<u32> {
    let value = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .unwrap_or(value);
    (value.len() == 6)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}
/// 旧 INI 数值字段；下个版本随旧解析器删除。
fn number(value: &str, min: f32, max: f32) -> Option<f32> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && (min..=max).contains(value))
}
fn width_limits(tool: Tool) -> (f32, f32) {
    match tool {
        Tool::LocalFill => (1., 50.),
        Tool::Spotlight => (0., 512.),
        _ => (0.25, 512.),
    }
}
/// 旧 INI 单字段解析；下个版本随旧解析器删除。
fn parse_field(tool: Tool, style: &mut Style, key: &str, value: &str) {
    macro_rules! set {
        ($target:expr, $value:expr) => {
            if let Some(value) = $value {
                $target = value;
            }
        };
    }
    match key {
        "color" => set!(style.color, color(value)),
        "width" => {
            let (min, max) = width_limits(tool);
            set!(style.width, number(value, min, max));
        }
        "blockSize" => set!(
            style.block_size,
            value.parse::<u32>().ok().filter(|v| (2..=128).contains(v))
        ),
        "opacity" => set!(style.opacity, number(value, 0., 1.)),
        "highlighterBlend" if matches!(tool, Tool::Highlighter | Tool::HighlighterRect) => {
            style.highlighter_blend = match value {
                "multiply" => HighlighterBlend::Multiply,
                _ => HighlighterBlend::Alpha,
            };
        }
        "highlighterShape" if tool == Tool::Highlighter => {
            style.highlighter_shape = match value {
                "line" => HighlighterShape::Line,
                _ => HighlighterShape::Freehand,
            };
        }
        "filled" => set!(style.filled, value.parse::<bool>().ok()),
        "radius" => set!(style.radius, number(value, 0., 2048.)),
        "rectangleRadii" if tool == Tool::Rectangle => {
            let parsed: Option<Vec<_>> = value
                .split(',')
                .map(|v| number(v.trim(), 0., 2048.))
                .collect();
            if let Some(r) = parsed.and_then(|r| <[f32; 4]>::try_from(r).ok()) {
                style.rectangle_radii = Some(r);
                style.radius = r[0];
            }
        }
        "effectRadius" if matches!(tool, Tool::Mosaic | Tool::Blur) => {
            set!(style.effect_radius, number(value, 0., 2048.));
        }
        "fontSize" => set!(style.font_size, number(value, 1., 1024.)),
        "bold" => set!(style.bold, value.parse::<bool>().ok()),
        "fontFamily"
            if !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control) =>
        {
            style.text.font = if value.eq_ignore_ascii_case("Microsoft YaHei UI") {
                FontFamily::default()
            } else {
                FontFamily::named(value)
            };
        }
        "italic" => set!(style.text.italic, value.parse::<bool>().ok()),
        "underline" => set!(style.text.underline, value.parse::<bool>().ok()),
        "strike" => set!(style.text.strike, value.parse::<bool>().ok()),
        "textAlignment" => set!(style.text.alignment, alignment_from(value)),
        "textOutline" => set!(style.text.outline, value.parse::<bool>().ok()),
        "textOutlineColor" => set!(style.text.outline_color, color(value)),
        "textOutlineWidth" => set!(style.text.outline_width, number(value, 0., 128.)),
        "textBackground" => set!(style.text.background, value.parse::<bool>().ok()),
        "textBackgroundColor" => set!(style.text.background_color, color(value)),
        "textBackgroundOpacity" => set!(style.text.background_opacity, number(value, 0., 1.)),
        "textBackgroundRadius" => set!(style.text.background_radius, number(value, 0., 2048.)),
        "arrowHead" => set!(style.arrow_head, arrow_from(value)),
        "arrowStart" => set!(style.arrow_start, value.parse::<bool>().ok()),
        "arrowEnd" => set!(style.arrow_end, value.parse::<bool>().ok()),
        "dashed" => set!(style.dashed, value.parse::<bool>().ok()),
        "pencilSmooth" => set!(style.pencil_smooth, value.parse::<bool>().ok()),
        "polylineSmooth" => set!(style.polyline_smooth, value.parse::<bool>().ok()),
        _ => {}
    }
}
/// 与解析使用相同边界；范围保留对象缩放产生的亚像素笔宽和较大字号。
fn validated(tool: Tool, mut style: Style, before: Style) -> Style {
    macro_rules! valid {
        ($target:expr, $old:expr, $min:expr, $max:expr) => {
            if !$target.is_finite() || !($min..=$max).contains(&$target) {
                $target = $old;
            }
        };
    }
    if style.color > 0xffffff {
        style.color = before.color;
    }
    if !(2..=128).contains(&style.block_size) {
        style.block_size = before.block_size;
    }
    let (width_min, width_max) = width_limits(tool);
    valid!(style.width, before.width, width_min, width_max);
    valid!(style.opacity, before.opacity, 0., 1.);
    valid!(style.radius, before.radius, 0., 2048.);
    if tool == Tool::Rectangle {
        if style.rectangle_radii.is_some_and(|r| {
            r.iter()
                .any(|v| !v.is_finite() || !(0. ..=2048.).contains(v))
        }) {
            style.rectangle_radii = before.rectangle_radii;
        }
    } else {
        style.rectangle_radii = None;
    }
    if matches!(tool, Tool::Mosaic | Tool::Blur) {
        valid!(style.effect_radius, before.effect_radius, 0., 2048.);
    } else {
        style.effect_radius = before.effect_radius;
    }
    valid!(style.font_size, before.font_size, 1., 1024.);
    valid!(
        style.text.outline_width,
        before.text.outline_width,
        0.,
        128.
    );
    valid!(
        style.text.background_opacity,
        before.text.background_opacity,
        0.,
        1.
    );
    valid!(
        style.text.background_radius,
        before.text.background_radius,
        0.,
        2048.
    );
    if style.text.outline_color > 0xffffff {
        style.text.outline_color = before.text.outline_color;
    }
    if style.text.background_color > 0xffffff {
        style.text.background_color = before.text.background_color;
    }
    if style.text.font.0 != 0 && crate::text_format::fonts().get(style.text.font.0).is_none() {
        style.text.font = before.text.font;
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[test]
    fn effect_radius_presets_keep_legacy_radius_and_roundtrip_only_plain_effect_tools() {
        let legacy = ToolPresets::parse_legacy_ini("[mosaic]\nradius=8\n[blur]\nradius=19\n");
        assert_eq!(legacy.style(Tool::Mosaic).radius, 8.);
        assert_eq!(legacy.style(Tool::Blur).radius, 19.);
        assert_eq!(legacy.style(Tool::Mosaic).effect_radius, 0.);
        assert_eq!(legacy.style(Tool::Blur).effect_radius, 0.);
        let presets = ToolPresets::parse_legacy_ini(
            "[mosaic]\nradius=8\neffectRadius=7.25\n[blur]\nradius=19\neffectRadius=2048\n[rectangle]\nradius=12\neffectRadius=9\n[mosaicBrush]\nwidth=21\neffectRadius=9\n[blurBrush]\nwidth=23\neffectRadius=9\n[localFill]\nwidth=17\neffectRadius=9\n[pixelEraser]\nwidth=31\neffectRadius=9\n",
        );
        assert_eq!(presets.style(Tool::Mosaic).effect_radius, 7.25);
        assert_eq!(presets.style(Tool::Blur).effect_radius, 2048.);
        assert_eq!(presets.style(Tool::Mosaic).radius, 8.);
        assert_eq!(presets.style(Tool::Blur).radius, 19.);
        assert_eq!(presets.style(Tool::Rectangle).radius, 12.);
        for tool in [
            Tool::Rectangle,
            Tool::MosaicBrush,
            Tool::BlurBrush,
            Tool::LocalFill,
            Tool::PixelEraser,
        ] {
            assert_eq!(presets.style(tool).effect_radius, 0.);
        }
        let wire = presets.encode_legacy_ini();
        assert_eq!(wire.matches("effectRadius=").count(), 2);
        let restored = ToolPresets::parse_legacy_ini(&wire);
        assert_eq!(restored.styles, presets.styles);
        assert!(!restored.is_dirty());
        assert_eq!(
            ToolPresets::parse_legacy_ini(&legacy.encode_legacy_ini()).styles,
            legacy.styles
        );
    }
    #[test]
    fn effect_radius_presets_invalid_fields_preserve_previous_value_or_default() {
        for section in ["mosaic", "blur"] {
            for invalid in ["-1", "2048.1", "NaN", "inf", "-inf", "unknown"] {
                let previous = ToolPresets::parse_legacy_ini(&format!(
                    "[{section}]\nradius=8\neffectRadius=13.5\neffectRadius={invalid}\n",
                ));
                let missing = ToolPresets::parse_legacy_ini(&format!(
                    "[{section}]\nradius=8\neffectRadius={invalid}\n",
                ));
                let tool = if section == "mosaic" {
                    Tool::Mosaic
                } else {
                    Tool::Blur
                };
                assert_eq!(
                    previous.style(tool).effect_radius,
                    13.5,
                    "{section}: {invalid}"
                );
                assert_eq!(
                    missing.style(tool).effect_radius,
                    0.,
                    "{section}: {invalid}"
                );
                assert_eq!(previous.style(tool).radius, 8.);
                assert_eq!(missing.style(tool).radius, 8.);
            }
            for valid in [0., 0.25, 2048.] {
                let parsed =
                    ToolPresets::parse_legacy_ini(&format!("[{section}]\neffectRadius={valid}\n"));
                let tool = if section == "mosaic" {
                    Tool::Mosaic
                } else {
                    Tool::Blur
                };
                assert_eq!(parsed.style(tool).effect_radius, valid);
            }
        }
    }
    #[test]
    fn effect_radius_preset_updates_keep_valid_fields_without_cross_tool_mutation() {
        for tool in [Tool::Mosaic, Tool::Blur] {
            let mut presets = ToolPresets::default();
            let before = Style {
                radius: 8.,
                effect_radius: 13.5,
                ..default_style(tool)
            };
            assert!(presets.update(tool, before));
            for invalid in [-1., 2048.1, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                assert!(!presets.update(
                    tool,
                    Style {
                        effect_radius: invalid,
                        ..before
                    }
                ));
                assert_eq!(presets.style(tool), before);
            }
            assert!(presets.update(
                tool,
                Style {
                    effect_radius: f32::NAN,
                    width: 7.,
                    ..before
                }
            ));
            assert_eq!(
                presets.style(tool),
                Style {
                    width: 7.,
                    ..before
                }
            );
            assert!(presets.update(
                tool,
                Style {
                    effect_radius: 0.,
                    ..before
                }
            ));
            assert_eq!(presets.style(tool).radius, 8.);
            assert_eq!(presets.style(tool).effect_radius, 0.);
            assert_eq!(
                ToolPresets::parse_legacy_ini(&presets.encode_legacy_ini()).styles,
                presets.styles
            );
        }
        for (tool, _) in TOOLS {
            if matches!(tool, Tool::Mosaic | Tool::Blur) {
                continue;
            }
            let mut presets = ToolPresets::default();
            let before = presets.style(tool);
            assert!(!presets.update(
                tool,
                Style {
                    effect_radius: 11.,
                    ..before
                }
            ));
            assert_eq!(presets.style(tool), before);
            assert!(!presets.is_dirty());
        }
    }
    #[test]
    fn local_fill_presets_wire_defaults_bounds_and_other_tools_are_isolated() {
        assert!(persistent_tool(Tool::LocalFill));
        assert_eq!(default_style(Tool::LocalFill).width, 10.);
        let parsed = ToolPresets::parse_legacy_ini(
            "[localFill]\nwidth=17.5\n[LocalFill]\nwidth=40\n[mosaicBrush]\nwidth=80\n",
        );
        assert_eq!(parsed.style(Tool::LocalFill).width, 17.5);
        assert_eq!(parsed.style(Tool::MosaicBrush).width, 80.);
        for invalid in ["0.25", "0", "50.1", "100", "NaN", "inf"] {
            assert_eq!(
                ToolPresets::parse_legacy_ini(&format!("[localFill]\nwidth={invalid}\n"))
                    .style(Tool::LocalFill)
                    .width,
                10.
            );
        }
        let mut presets = parsed;
        let before = presets.style(Tool::LocalFill);
        assert!(!presets.update(
            Tool::LocalFill,
            Style {
                width: 100.,
                ..before
            }
        ));
        assert_eq!(presets.style(Tool::LocalFill), before);
        assert!(presets.update(
            Tool::LocalFill,
            Style {
                width: 50.,
                ..before
            }
        ));
        let wire = presets.encode_legacy_ini();
        assert!(wire.contains("\n[localFill]\n"));
        assert!(!wire.contains("\n[LocalFill]\n"));
        let restored = ToolPresets::parse_legacy_ini(&wire);
        assert_eq!(restored.style(Tool::LocalFill).width, 50.);
        assert_eq!(restored.style(Tool::MosaicBrush).width, 80.);
        assert_eq!(restored.style(Tool::Mosaic), default_style(Tool::Mosaic));
    }
    #[test]
    fn highlighter_modes_presets_old_sections_keep_alpha_and_new_tools_use_multiply() {
        let legacy = ToolPresets::parse_legacy_ini("[highlighter]\nwidth=31\n[highlighterRect]\n");
        let missing = ToolPresets::parse_legacy_ini("[pen]\nwidth=7\n");
        for tool in [Tool::Highlighter, Tool::HighlighterRect] {
            let old = legacy.style(tool);
            assert_eq!(old.highlighter_blend, HighlighterBlend::Alpha);
            assert_eq!(old.highlighter_shape, HighlighterShape::Freehand);
            assert_eq!(old.opacity, 0.35);
            assert_eq!(old.color, 0xffd43b);
            let new = missing.style(tool);
            assert_eq!(new.highlighter_blend, HighlighterBlend::Multiply);
            assert_eq!(new.highlighter_shape, HighlighterShape::Freehand);
            assert_eq!(new.opacity, 1.);
            assert_eq!(new.width, 24.);
        }
        assert_eq!(legacy.style(Tool::Highlighter).width, 31.);
        let watermark = default_style(Tool::Watermark);
        assert_eq!(watermark.opacity, 0.35);
        assert_eq!(watermark.highlighter_blend, HighlighterBlend::Alpha);
        assert_eq!(watermark.color, Style::default().color);
    }
    #[test]
    fn highlighter_modes_presets_invalid_values_fall_back_without_cross_tool_fields() {
        let presets = ToolPresets::parse_legacy_ini(
            "[highlighter]\nhighlighterBlend=multiply\nhighlighterBlend=unknown\nhighlighterShape=line\nhighlighterShape=unknown\nopacity=0.7\nwidth=30\n[highlighterRect]\nhighlighterBlend=unknown\nhighlighterShape=line\n[pen]\nhighlighterBlend=multiply\nhighlighterShape=line\n",
        );
        let pen = presets.style(Tool::Highlighter);
        assert_eq!(pen.highlighter_blend, HighlighterBlend::Alpha);
        assert_eq!(pen.highlighter_shape, HighlighterShape::Freehand);
        assert_eq!(pen.opacity, 0.7);
        assert_eq!(pen.width, 30.);
        let rect = presets.style(Tool::HighlighterRect);
        assert_eq!(rect.highlighter_blend, HighlighterBlend::Alpha);
        assert_eq!(rect.highlighter_shape, HighlighterShape::Freehand);
        assert_eq!(presets.style(Tool::Pen), default_style(Tool::Pen));
    }
    #[test]
    fn highlighter_modes_presets_round_trip_explicit_mode_shape_and_opacity() {
        let mut presets = ToolPresets::default();
        let pen = Style {
            opacity: 0.72,
            highlighter_shape: HighlighterShape::Line,
            ..default_style(Tool::Highlighter)
        };
        let rect = Style {
            opacity: 0.41,
            radius: 17.,
            highlighter_blend: HighlighterBlend::Alpha,
            ..default_style(Tool::HighlighterRect)
        };
        assert!(presets.update(Tool::Highlighter, pen));
        assert!(presets.update(Tool::HighlighterRect, rect));
        assert!(presets.update(
            Tool::Pen,
            Style {
                width: 9.,
                ..default_style(Tool::Pen)
            }
        ));
        let wire = presets.encode_legacy_ini();
        assert!(wire.contains("highlighterBlend=multiply\n"));
        assert!(wire.contains("highlighterBlend=alpha\n"));
        assert!(wire.contains("highlighterShape=line\n"));
        assert_eq!(wire.matches("highlighterBlend=").count(), 2);
        assert_eq!(wire.matches("highlighterShape=").count(), 1);
        let restored = ToolPresets::parse_legacy_ini(&wire);
        assert_eq!(restored.styles, presets.styles);
        assert!(!restored.is_dirty());
        let legacy = ToolPresets::parse_legacy_ini("[highlighterRect]\n");
        assert_eq!(
            ToolPresets::parse_legacy_ini(&legacy.encode_legacy_ini()).styles,
            legacy.styles
        );
    }
    #[test]
    fn tool_presets_repeated_session_flush_preserves_other_window_changes() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(format!("tool-presets-repeat-{}", std::process::id()));
        let path = dir.join("tool-presets.json");
        let mut a = ToolPresets::load_at(&path).unwrap();
        let mut b = ToolPresets::load_at(&path).unwrap();
        let mut editor = crate::editor::Editor::new(crate::geometry::Rect::full(800, 600), None);
        editor.restore_tool_styles(b.styles());
        let rect = Style {
            radius: 20.,
            ..default_style(Tool::Rectangle)
        };
        a.update(Tool::Rectangle, rect);
        a.save_at(&path).unwrap();
        editor.select_tool(Tool::Pen);
        editor.set_width(7.);
        for (t, s) in editor.tool_styles() {
            b.update(t, s);
        }
        b.save_at(&path).unwrap();
        // 模拟同一旧 Editor 随后的输出/关闭刷新，不应产生任何脏工具。
        for (t, s) in editor.tool_styles() {
            b.update(t, s);
        }
        assert!(!b.is_dirty());
        assert!(!b.save_at(&path).unwrap());
        let loaded = ToolPresets::load_at(&path).unwrap();
        assert_eq!(loaded.style(Tool::Rectangle), rect);
        assert_eq!(loaded.style(Tool::Pen).width, 7.);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn tool_presets_round_trip_independent_styles_and_explicit_fields() {
        let mut presets = ToolPresets::default();
        for (index, (tool, _)) in TOOLS.iter().enumerate() {
            let mut style = default_style(*tool);
            style.color = 0x123456 + index as u32;
            style.width = 0.5 + index as f32;
            style.opacity = 0.6;
            style.block_size = 32;
            style.filled = true;
            style.radius = 8.;
            style.font_size = 80.;
            style.bold = true;
            style.text.italic = true;
            style.text.underline = true;
            style.text.strike = true;
            style.text.alignment = Alignment::Center;
            style.text.outline = true;
            style.text.outline_color = 0xabcdef;
            style.text.outline_width = 1.5;
            style.text.background = true;
            style.text.background_color = 0x456789;
            style.text.background_opacity = 0.3;
            style.text.background_radius = 10.;
            style.arrow_head = ArrowHead::Dart;
            style.arrow_start = true;
            style.arrow_end = false;
            style.dashed = true;
            style.pencil_smooth = false;
            style.polyline_smooth = true;
            assert!(presets.update(*tool, style));
        }
        let wire = presets.encode_legacy_ini();
        assert!(wire.contains("[highlighterRect]\n"));
        assert!(wire.contains("fontFamily=Microsoft YaHei UI\n"));
        assert!(!wire.contains("textBody=") && !wire.contains("fontIndex="));
        let loaded = ToolPresets::parse_legacy_ini(&wire);
        assert_eq!(loaded.styles, presets.styles);
        assert!(!loaded.is_dirty());
        // JSON 往返同样逐字段保真；旧 INI 迁移得到相同的 JSON。
        let json = ToolPresets::from_file(presets.file());
        assert_eq!(json.styles, presets.styles);
        assert!(!json.is_dirty());
        let dir = crate::config_json::TestDir::new("tool-presets");
        let path = dir.join("tool-presets.json");
        std::fs::write(dir.join("tool-presets.ini"), &wire).unwrap();
        assert_eq!(ToolPresets::load_at(&path).unwrap().styles, presets.styles);
        assert!(path.exists() && dir.join("tool-presets.ini.bak").exists());
        assert!(!dir.join("tool-presets.ini").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schema_version\": 1") && text.contains("\"highlighterRect\": {"));
        assert_eq!(ToolPresets::load_at(&path).unwrap().styles, presets.styles);
        assert!(!presets.update(Tool::Crop, Style::default()));
        assert!(!presets.update(Tool::Edit, Style::default()));
        assert!(!presets.update(Tool::Lasso, Style::default()));
    }
    #[test]
    fn tool_presets_reject_corrupt_values_without_losing_valid_fields_or_defaults() {
        let text = "version=42\n[pen]\ncolor=#123456\nwidth=NaN\nopacity=inf\nblockSize=4294967295\nradius=-1\nfontSize=999999\nbold=garbage\ntextBackgroundOpacity=-0.2\ntextOutlineWidth=NaN\ntextBackgroundRadius=inf\narrowHead=unknown\ntextAlignment=unknown\ntextOutlineColor=ffffffffff\nfutureField=anything\n[futureTool]\nwidth=12\n[highlighter]\npencilSmooth=false\n";
        let presets = ToolPresets::parse_legacy_ini(text);
        let pen = presets.style(Tool::Pen);
        let expected = Style {
            color: 0x123456,
            ..default_style(Tool::Pen)
        };
        assert_eq!(pen, expected);
        let highlighter = presets.style(Tool::Highlighter);
        assert_eq!(highlighter.width, 24.);
        assert_eq!(highlighter.opacity, 0.35);
        assert!(!highlighter.pencil_smooth);
        let mut current = presets;
        let mut invalid = pen;
        invalid.width = f32::NEG_INFINITY;
        invalid.color = u32::MAX;
        invalid.text.background_color = u32::MAX;
        invalid.text.background_radius = f32::NAN;
        invalid.dashed = true;
        assert!(current.update(Tool::Pen, invalid));
        assert_eq!(
            current.style(Tool::Pen),
            Style {
                dashed: true,
                ..pen
            }
        );
    }
    #[test]
    fn tool_presets_batch_save_merges_sessions_and_does_not_write_on_updates() {
        static TEST_ID: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(format!(
                "tool-presets-{}-{}",
                std::process::id(),
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ));
        let path = dir.join("tool-presets.json");
        let mut a = ToolPresets::load_at(&path).unwrap();
        let mut b = ToolPresets::load_at(&path).unwrap();
        let pen = Style {
            width: 7.,
            ..default_style(Tool::Pen)
        };
        assert!(a.update(Tool::Pen, pen));
        assert!(!path.exists(), "滚轮等内存修改不应创建配置");
        assert!(a.save_at(&path).unwrap());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!a.save_at(&path).unwrap());
        assert_eq!(
            modified,
            std::fs::metadata(&path).unwrap().modified().unwrap()
        );
        let rect = Style {
            radius: 20.,
            ..default_style(Tool::Rectangle)
        };
        assert!(b.update(Tool::Rectangle, rect));
        assert!(b.save_at(&path).unwrap());
        let c = ToolPresets::load_at(&path).unwrap();
        assert_eq!(c.style(Tool::Pen), pen);
        assert_eq!(c.style(Tool::Rectangle), rect);
        assert!(a.update(Tool::Pen, default_style(Tool::Pen)));
        assert!(a.save_at(&path).unwrap());
        let c = ToolPresets::load_at(&path).unwrap();
        assert_eq!(c.style(Tool::Pen), default_style(Tool::Pen));
        assert_eq!(c.style(Tool::Rectangle), rect);
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            ToolPresets::load_at(&path).unwrap().style(Tool::Pen),
            default_style(Tool::Pen)
        );
        // 不递归删除：只清理本测试明确创建的两个文件。
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn tool_presets_json_invalid_fields_keep_previous_value_or_default() {
        let file: PresetsFile = serde_json::from_str(
            r#"{"tools":{
                "pen":{"width":7,"color":"bad","dashed":"yes","arrow_head":"future"},
                "rectangle":{"rectangle_radii":[3,8,15,22],"opacity":2},
                "spotlight":{"rectangle_radii":[1,2,3,4],"width":-1},
                "highlighter":{"highlighter_blend":"multiply","highlighter_shape":"line"},
                "highlighterRect":{"highlighter_shape":"line"},
                "future":{"width":3},
                "mosaic":"not an object"
            }}"#,
        )
        .unwrap();
        let presets = ToolPresets::from_file(file);
        let pen = presets.style(Tool::Pen);
        assert_eq!(pen.width, 7.);
        let default_pen = default_style(Tool::Pen);
        assert_eq!(
            (pen.color, pen.dashed, pen.arrow_head),
            (
                default_pen.color,
                default_pen.dashed,
                default_pen.arrow_head
            )
        );
        let rectangle = presets.style(Tool::Rectangle);
        assert_eq!(rectangle.rectangle_radii, Some([3., 8., 15., 22.]));
        assert_eq!(rectangle.radius, 3.);
        assert_eq!(rectangle.opacity, default_style(Tool::Rectangle).opacity);
        assert_eq!(
            presets.style(Tool::Spotlight),
            default_style(Tool::Spotlight)
        );
        let highlighter = presets.style(Tool::Highlighter);
        assert_eq!(highlighter.highlighter_blend, HighlighterBlend::Multiply);
        assert_eq!(highlighter.highlighter_shape, HighlighterShape::Line);
        // 旧节语义：出现过的高亮矩形节从 alpha 默认起读，形状字段不适用。
        let rect = presets.style(Tool::HighlighterRect);
        assert_eq!(rect.highlighter_blend, HighlighterBlend::Alpha);
        assert_eq!(
            rect.highlighter_shape,
            default_style(Tool::HighlighterRect).highlighter_shape
        );
        assert_eq!(presets.style(Tool::Mosaic), default_style(Tool::Mosaic));
    }
}

#[cfg(test)]
mod rectangle_corner_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[test]
    fn rectangle_corner_presets_keep_legacy_linked_and_validate_only_rectangle_values() {
        let legacy = ToolPresets::parse_legacy_ini("[rectangle]\nradius=17\n");
        assert_eq!(legacy.style(Tool::Rectangle).rectangle_radii, None);
        assert_eq!(legacy.style(Tool::Rectangle).radius, 17.);
        let initial=ToolPresets::parse_legacy_ini("[rectangle]\nrectangleRadii=3,8,15,22\n[spotlight]\nrectangleRadii=1,2,3,4\n[highlighterRect]\nrectangleRadii=1,2,3,4\n[mosaic]\neffectRadius=7\nrectangleRadii=1,2,3,4\n");
        assert_eq!(
            initial.style(Tool::Rectangle).rectangle_radii,
            Some([3., 8., 15., 22.])
        );
        for tool in [Tool::Spotlight, Tool::HighlighterRect, Tool::Mosaic] {
            assert_eq!(initial.style(tool).rectangle_radii, None);
        }
        assert_eq!(initial.style(Tool::Mosaic).effect_radius, 7.);
        assert_eq!(
            initial
                .encode_legacy_ini()
                .matches("rectangleRadii=")
                .count(),
            1
        );
        let roundtrip = ToolPresets::parse_legacy_ini(&initial.encode_legacy_ini());
        assert_eq!(
            roundtrip.style(Tool::Rectangle),
            initial.style(Tool::Rectangle)
        );
        for invalid in [
            "1,2,3",
            "1,2,3,4,5",
            "1,-2,3,4",
            "1,NaN,3,4",
            "1,inf,3,4",
            "1,2049,3,4",
        ] {
            let parsed = ToolPresets::parse_legacy_ini(&format!(
                "[rectangle]\nrectangleRadii=3,8,15,22\nrectangleRadii={invalid}\n"
            ));
            assert_eq!(
                parsed.style(Tool::Rectangle).rectangle_radii,
                Some([3., 8., 15., 22.])
            );
        }
        let mut presets = initial;
        let before = presets.style(Tool::Rectangle);
        let mut invalid = before;
        invalid.rectangle_radii = Some([3., f32::NAN, 15., 22.]);
        assert!(!presets.update(Tool::Rectangle, invalid));
        assert_eq!(presets.style(Tool::Rectangle), before);
    }
    #[test]
    fn rectangle_corner_preset_save_restore_and_return_to_linked_are_actual_atomic_file_updates() {
        static TEST_ID: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(format!(
                "rectangle-corner-presets-{}-{}",
                std::process::id(),
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ));
        let path = dir.join("tool-presets.json");
        let mut presets = ToolPresets::load_at(&path).unwrap();
        let rectangle = Style {
            radius: 3.,
            rectangle_radii: Some([3., 8., 15., 22.]),
            ..default_style(Tool::Rectangle)
        };
        assert!(presets.update(Tool::Rectangle, rectangle));
        assert!(presets.save_at(&path).unwrap());
        let loaded = ToolPresets::load_at(&path).unwrap();
        assert_eq!(loaded.style(Tool::Rectangle), rectangle);
        let linked = Style {
            radius: 17.,
            rectangle_radii: None,
            ..rectangle
        };
        assert!(presets.update(Tool::Rectangle, linked));
        assert!(presets.save_at(&path).unwrap());
        assert_eq!(
            ToolPresets::load_at(&path).unwrap().style(Tool::Rectangle),
            linked
        );
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("rectangleRadii="));
        assert!(!presets.save_at(&path).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
