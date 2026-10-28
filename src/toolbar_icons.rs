//! 统一的 SVG 图标入口：资源随 EXE 内嵌，GPUI 按显示尺寸缓存渲染结果。
use crate::{annotation::Tool, ocr_markup::TextMark};
use gpui::{
    prelude::*, px, rgb, rgba, svg, AssetSource, HighlightStyle, SharedString, StrikethroughStyle,
    StyledText, Svg, UnderlineStyle,
};
use std::borrow::Cow;

/// OCR标记浮条和菜单共用的黄色、红色、蓝色、绿色；改变主题不改变输出RGB。
pub(crate) const TEXT_MARK_COLORS: [u32; 4] = [0xffd54f, 0xff4b5c, 0x378de5, 0x30b884];

macro_rules! define_icons {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub enum Icon { $($variant),+ }
        impl Icon {
            fn path(self) -> &'static str {
                match self { $(Self::$variant => concat!("icons/", $name, ".svg")),+ }
            }
        }
        pub struct IconAssets;
        impl AssetSource for IconAssets {
            fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
                Ok(match path {
                    $(concat!("icons/", $name, ".svg") => Some(Cow::Borrowed(
                        include_bytes!(concat!("../assets/icons/", $name, ".svg")).as_slice()
                    )),)+
                    _ => None,
                })
            }
            fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
                Ok([$(concat!("icons/", $name, ".svg")),+]
                    .into_iter().filter(|name| name.starts_with(path)).map(Into::into).collect())
            }
        }
    };
}
define_icons! {
    Crop => "crop",
    Cut => "cut",
    SelectionRange => "selection-range",
    Reselect => "reselect",
    Refresh => "refresh",
    Edit => "edit",
    Pen => "pen",
    Rectangle => "rectangle",
    Ellipse => "ellipse",
    Triangle => "triangle",
    Diamond => "diamond",
    Star => "star",
    Arrow => "arrow",
    Polyline => "polyline",
    Highlighter => "highlighter",
    HighlighterRect => "highlighter-rect",
    Text => "text",
    Watermark => "watermark",
    Serial => "serial",
    Mosaic => "mosaic",
    Blur => "blur",
    MosaicBrush => "mosaic-brush",
    BlurBrush => "blur-brush",
    LocalFill => "local-fill",
    Spotlight => "spotlight",
    Eraser => "eraser",
    ObjectEraser=>"object-eraser",
    Lasso=>"lasso",
    Undo => "undo",
    Redo => "redo",
    RotateLeft => "rotate-left",
    RotateRight => "rotate-right",
    More => "more",
    Theme => "theme",
    Save => "save",
    Copy => "copy",
    LongCapture => "long-capture",
    Pause => "pause",
    Play => "play",
    AutoDown => "auto-down",
    AutoUp => "auto-up",
    AutoLeft => "auto-left",
    AutoRight => "auto-right",
    AxisVertical => "axis-vertical",
    AxisHorizontal => "axis-horizontal",
    Settings => "settings",
    Capture => "capture",
    Speed => "speed",
    Pin => "pin",
    Ocr => "ocr",
    OcrTranslate => "ocr-translate",
    Table => "table",
    Formula => "formula",
    Translate => "translate",
    Qr => "qr",
    SmartMosaic => "smart-mosaic",
    Confirm => "confirm",
    Close => "close",
    Grip => "grip",
    Move => "move",
    Chevron => "chevron",
    ChevronUp => "chevron-up",
    Keyboard => "keyboard",
    Toolbar => "toolbar",
    Video => "video",
    Record => "record",
    Markup => "markup",
    Stop => "stop",
    Dot => "dot",
    Volume => "volume",
    VolumeMute => "volume-mute",
    History => "history",
    Search => "search",
    Filter => "filter",
    Shield => "shield",
    // 二级栏控件（20×20 网格，16 像素显示），由 docs/design/annotation-tools.html 的 <symbol> 转出。
    FillStroke => "fill-stroke",
    FillSolid => "fill-solid",
    LineSolid => "line-solid",
    LineDash => "line-dash",
    Radius => "radius",
    Opacity => "opacity",
    Smooth => "smooth",
    FontSize => "font-size",
    TextStyle => "text-style",
    EndNone => "end-none",
    EndSingle => "end-single",
    EndBoth => "end-both",
    HeadOpen => "head-open",
    HeadTriangle => "head-triangle",
    HeadDart => "head-dart",
    BlendMultiply => "blend-multiply",
    BlendAlpha => "blend-alpha",
    AlignLeft => "align-left",
    AlignCenter => "align-center",
    AlignRight => "align-right",
    Bold => "bold",
    Italic => "italic",
    Underline => "underline",
    Strike => "strike",
    TextOutline => "text-outline",
    TextBackground => "text-background",
    RegionRect => "region-rect",
    Sliders => "sliders",
    DeleteVertex => "delete-vertex",
    ClosedFill => "closed-fill",
    Line => "line",
    Group => "group",
    Ungroup => "ungroup",
    EnterGroup => "enter-group",
    ParentGroup => "parent-group",
    BringFront => "bring-front",
    SendBack => "send-back",
    Duplicate => "duplicate",
    Trash => "trash",
    Layers => "layers",
    Check => "check",
    Sparkle => "sparkle",
    Monitor => "monitor",
    Monitors => "monitors",
    Guides => "guides",
    // 主界面贴图页：显示／隐藏、锁定、鼠标穿透、定位。
    Eye => "eye",
    EyeOff => "eye-off",
    Lock => "lock",
    Unlock => "unlock",
    ClickThrough => "click-through",
    Target => "target",
    // 主界面工具页。
    Apps => "apps",
    Image => "image",
    Images => "images",
    Compare => "compare",
    Collage => "collage",
    File => "file",
    // 图片查看工具栏。
    Folder => "folder",
    ChevronLeft => "chevron-left",
    ChevronRight => "chevron-right",
    Fit => "fit",
    Fullscreen => "fullscreen",
    Step => "step",
    Grid => "grid",
    Eyedropper => "eyedropper",
}

pub fn action(icon: Icon, color: u32) -> Svg {
    svg()
        .path(icon.path())
        .size(px(20.))
        .flex_shrink_0()
        .text_color(rgb(color))
}

pub fn icon(tool: Tool, color: u32) -> Svg {
    action(
        match tool {
            Tool::Crop => Icon::Crop,
            Tool::Edit => Icon::Edit,
            Tool::Pen => Icon::Pen,
            Tool::Rectangle => Icon::Rectangle,
            Tool::Ellipse => Icon::Ellipse,
            Tool::Triangle => Icon::Triangle,
            Tool::Diamond => Icon::Diamond,
            Tool::Star => Icon::Star,
            Tool::Arrow => Icon::Arrow,
            Tool::Polyline => Icon::Polyline,
            Tool::Highlighter => Icon::Highlighter,
            Tool::HighlighterRect => Icon::HighlighterRect,
            Tool::Text => Icon::Text,
            Tool::Watermark => Icon::Watermark,
            Tool::Serial => Icon::Serial,
            Tool::Mosaic => Icon::Mosaic,
            Tool::SmartMosaic => Icon::SmartMosaic,
            Tool::Blur => Icon::Blur,
            Tool::MosaicBrush => Icon::MosaicBrush,
            Tool::BlurBrush => Icon::BlurBrush,
            Tool::LocalFill => Icon::LocalFill,
            Tool::Spotlight => Icon::Spotlight,
            Tool::Eraser => Icon::ObjectEraser,
            Tool::PixelEraser => Icon::Eraser,
            Tool::Lasso => Icon::Lasso,
        },
        color,
    )
}

/// 原位OCR共用的视觉符号；文字内容与标记输出不在这里处理。
pub(crate) fn text_mark(mark: TextMark, color: u32) -> StyledText {
    let (symbol, style) = match mark {
        TextMark::Highlight => (
            " A ",
            HighlightStyle {
                background_color: Some(rgba((color << 8) | 0xa0).into()),
                ..Default::default()
            },
        ),
        TextMark::Underline | TextMark::Wave => (
            if mark == TextMark::Wave { "W" } else { "U" },
            HighlightStyle {
                underline: Some(UnderlineStyle {
                    thickness: px(if mark == TextMark::Wave { 1.5 } else { 2. }),
                    color: Some(rgb(color).into()),
                    wavy: mark == TextMark::Wave,
                }),
                ..Default::default()
            },
        ),
        TextMark::Strike => (
            "S",
            HighlightStyle {
                strikethrough: Some(StrikethroughStyle {
                    thickness: px(2.),
                    color: Some(rgb(color).into()),
                }),
                ..Default::default()
            },
        ),
    };
    StyledText::new(symbol).with_highlights([(0..symbol.len(), style)])
}
