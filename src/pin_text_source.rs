//! 纯文字贴图是矢量文档：纯色底由画布直接绘制，不生成底图位图；
//! 底色与默认文字色实时跟随界面主题，只在复制、保存、拖出时渲染成图片。
//! 文档与磁盘记录统一保存浅色主题下的颜色，打开和换主题时再映射到当前主题。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    pin_model::PinState,
    pin_view::PinView,
    text_layout::TextLayout,
    Result,
};
use gpui::{App, WindowHandle};

const LIGHT_BACKGROUND: u32 = 0xffffff;
const LIGHT_FOREGROUND: u32 = 0x20252b;
const DARK_BACKGROUND: u32 = 0x1d222a;
const DARK_FOREGROUND: u32 = 0xd9e2e9;

/// 当前主题下的（底色, 默认文字色），均为 0xRRGGBB。
pub(crate) fn colors(light: bool) -> (u32, u32) {
    if light {
        (LIGHT_BACKGROUND, LIGHT_FOREGROUND)
    } else {
        (DARK_BACKGROUND, DARK_FOREGROUND)
    }
}
/// 画布与导出使用的 BGRA 纯色底。
pub(crate) fn fill(light: bool) -> [u8; 4] {
    let [_, r, g, b] = colors(light).0.to_be_bytes();
    [b, g, r, 255]
}
/// 只改写仍为主题默认色的文字；用户自选颜色、保留来源整卡配色的富文本保持不变。返回是否改变。
pub(crate) fn remap_theme(annotation: &mut Annotation, from_light: bool, to_light: bool) -> bool {
    let from = colors(from_light).1;
    let to = colors(to_light).1;
    if from == to
        || annotation.tool != Tool::Text
        || annotation.style.color != from
        || annotation
            .rich
            .as_ref()
            .is_some_and(|rich| rich.card_fill.is_some())
    {
        return false;
    }
    annotation.style.color = to;
    true
}

/// 新建文字执行原初始化插入；恢复必须安装完整 Document（包括空 current）。
pub(crate) enum TextInstallation {
    New(Vec<Annotation>),
    Restore(Box<crate::pin_text_document::WireTextDocument>),
}

pub(crate) struct TextSource {
    pub(crate) size: (u32, u32),
    pub(crate) annotation: Annotation,
}
pub(crate) fn validate_size(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > 65535
        || height > 65535
        || u64::from(width) * u64::from(height) > 32_000_000
    {
        return Err("文字贴图尺寸无效".into());
    }
    Ok(())
}
pub(crate) fn prepare(text: String) -> Result<TextSource> {
    if text.trim().is_empty() || text.len() > 32768 {
        return Err(tr!(
            "文本贴图支持非空、最多 32 KiB 文本，请分段贴图",
            "Text pins need non-empty text up to 32 KiB; split it up"
        )
        .into());
    }
    let mut annotation = Annotation::new(
        Tool::Text,
        Point { x: 18., y: 18. },
        Style {
            color: LIGHT_FOREGROUND,
            font_size: 22.,
            ..Default::default()
        },
    );
    annotation.update(Point { x: 680., y: 16300. });
    annotation.text = text;
    let height = (TextLayout::new(&annotation)?.height.ceil() as u32 + 36).max(72);
    if height > 16000 {
        return Err(tr!(
            "文本贴图过长，请分段贴图",
            "Text is too long for one pin; split it up"
        )
        .into());
    }
    // 收紧对象高度，避免在编辑时出现远在文字下面的控制点。
    annotation.points[1].y = height.saturating_sub(18) as f32;
    validate_size(700, height)?;
    Ok(TextSource {
        size: (700, height),
        annotation,
    })
}
/// 矢量贴图不读取 original/image 像素；只保留 1×1 占位以满足窗口内容身份。
pub(crate) fn placeholder() -> Image {
    Image {
        width: 1,
        height: 1,
        bgra: vec![255; 4],
        readback_ms: 0.,
    }
}

impl PinView {
    /// 安装浅色基准的文字对象，并映射到本贴图当前主题。
    pub(crate) fn install_text_annotations(
        &mut self,
        annotations: Vec<Annotation>,
        w: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let light = self.vector_text.unwrap_or(true);
        self.ensure_canvas(w, cx)?;
        let canvas = self.canvas.as_ref().ok_or("文字贴图画布尚未创建")?;
        canvas.update(cx, |v, cx| {
            for mut annotation in annotations {
                remap_theme(&mut annotation, true, light);
                v.editor.document.insert(annotation);
            }
            v.editor.select_tool(Tool::Crop);
            cx.notify();
        });
        self.set_canvas_active(false, w, cx)?;
        self.refresh_native_text_selection(cx)?;
        // 短文字贴图高度可能只有 72px，常驻提示会盖住最后一行。
        self.status.clear();
        cx.notify();
        Ok(())
    }
    pub(crate) fn install_text_document(
        &mut self,
        document: &crate::pin_text_document::WireTextDocument,
        w: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let light = self.vector_text.ok_or("文字文档恢复缺少纯色来源")?;
        self.ensure_canvas(w, cx)?;
        let canvas = self.canvas.as_ref().ok_or("文字贴图画布尚未创建")?;
        canvas.update(cx, |v, cx| -> Result<()> {
            document.restore_into(&mut v.editor, light)?;
            v.source.set_solid_fill(fill(light));
            v.editor.select_tool(Tool::Crop);
            v.scene_stamp = None;
            v.sync_viewport(w);
            cx.notify();
            Ok(())
        })?;
        self.set_canvas_active(false, w, cx)?;
        self.refresh_native_text_selection(cx)?;
        self.status.clear();
        cx.notify();
        Ok(())
    }
    pub(crate) fn vector_text_fill(&self) -> Option<[u8; 4]> {
        self.vector_text.map(fill)
    }
    /// 系统或设置切换明暗后，底色和仍为默认色的文字一起换色；不进入撤销历史。
    pub(crate) fn sync_text_theme(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(current) = self.vector_text else {
            return;
        };
        let light = crate::theme::is_light(cx);
        if current == light {
            return;
        }
        self.vector_text = Some(light);
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, cx| {
                v.source.set_solid_fill(fill(light));
                v.editor
                    .document
                    .remap_silently(|a| remap_theme(a, current, light));
                if let Some(draft) = &mut v.editor.text_draft {
                    remap_theme(&mut draft.annotation, current, light);
                }
                v.scene_stamp = None;
                cx.notify();
            });
        }
        if let Err(error) = self.refresh_native_text_selection(cx) {
            self.status = error.to_string();
        }
    }
    pub(crate) fn open_text_source(
        source: TextSource,
        origin: (i32, i32),
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let state = PinState {
            zoom: crate::pin_model::fit(source.size, crate::pin_native::work_area(origin)),
            ..Default::default()
        };
        Self::open_vector_text(
            source.size,
            vec![source.annotation],
            origin,
            state,
            None,
            cx,
        )
    }
    pub(crate) fn open_text_document(
        document: &crate::pin_text_document::WireTextDocument,
        origin: (i32, i32),
        state: PinState,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_text_document_with_window(document, origin, state, None, cx)
    }
    pub(crate) fn open_text_document_with_window(
        document: &crate::pin_text_document::WireTextDocument,
        origin: (i32, i32),
        state: PinState,
        window: Option<crate::pin_store::PresentationWindow>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        document.validate()?;
        Self::open_vector_text_with(
            (document.width, document.height),
            TextInstallation::Restore(Box::new(document.clone())),
            origin,
            state,
            None,
            window,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_pin_keeps_original_unicode_whitespace_without_a_background_bitmap() {
        let text = "贴图 RustShot\r\n\t清晰文字  e\u{301} 🧑‍💻";
        let source = prepare(text.into()).unwrap();
        assert_eq!(source.annotation.text, text);
        assert_eq!(source.annotation.tool, Tool::Text);
        assert_eq!(source.annotation.style.color, colors(true).1);
        assert_eq!(source.size.0, 700);
        assert!(source.size.1 >= 72);
        let layout = TextLayout::new(&source.annotation).unwrap();
        assert!(layout.height + 36. <= source.size.1 as f32);
        assert_eq!(
            source.annotation.points[1].y,
            source.size.1.saturating_sub(18) as f32
        );
    }

    #[test]
    fn text_pin_rejects_empty_oversize_and_too_tall_source() {
        for text in ["  \r\n\t".into(), "字".repeat(10923), "a\n".repeat(1000)] {
            assert!(prepare(text).is_err());
        }
    }

    #[test]
    fn text_pin_theme_remaps_only_default_text_color_and_round_trips() {
        let mut text = prepare("主题".into()).unwrap().annotation;
        assert!(remap_theme(&mut text, true, false));
        assert_eq!(text.style.color, colors(false).1);
        assert!(!remap_theme(&mut text, false, false));
        assert!(remap_theme(&mut text, false, true));
        assert_eq!(text.style.color, colors(true).1);

        let mut custom = text.clone();
        custom.style.color = 0xff0000;
        assert!(!remap_theme(&mut custom, true, false));
        assert_eq!(custom.style.color, 0xff0000);

        let mut pen = Annotation::new(
            Tool::Pen,
            Point { x: 1., y: 1. },
            Style {
                color: colors(true).1,
                ..Default::default()
            },
        );
        assert!(!remap_theme(&mut pen, true, false));
        assert_eq!(fill(true), [255, 255, 255, 255]);
        assert_eq!(fill(false), [0x2a, 0x22, 0x1d, 255]);
    }
}
