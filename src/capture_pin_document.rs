//! Capture→Pin 的真实文档载荷；不合成Scene，不移位对象，不保留截图设备。
use crate::{
    canvas_source::CanvasSource,
    cursor::CapturedCursor,
    editor::Editor,
    geometry::Rect,
    pin_presentation::{PinPresentation, ScreenshotPinPayload},
    pin_raster_document::{
        capture_source::{CapturePixels, CaptureSourceMetadata},
        WireRasterDocument,
    },
    scene::Scene,
    selection_finish::SelectionFinishStyle,
    Result,
};
use gpui::RenderImage;
use std::sync::Arc;

pub(crate) struct CaptureDocumentPayload {
    visible: ScreenshotPinPayload,
    pixels: Arc<CapturePixels>,
    document: WireRasterDocument,
}
impl CaptureDocumentPayload {
    pub(crate) fn prepare(
        source: &CanvasSource,
        previews: &[Arc<RenderImage>],
        editor: &Editor,
        scene: &Scene,
        selected: Rect,
        cursor: Option<&CapturedCursor>,
        style: SelectionFinishStyle,
    ) -> Result<Self> {
        if !scene.errors.is_empty() {
            return Err(scene.errors.join(";").into());
        }
        if source.capture_pixels().is_some()
            && selected.intersection(source.screen_rect(0)) != Some(selected)
        {
            return Err("贴图再次交接范围超出固定可见内容域".into());
        }
        PinPresentation::new(source.bounds, selected, style)?;
        // 两栈/样式/几何预算先检查；来源只克隆CPU强引用，不读取截图设备。
        let pixels = source.shared_cpu_planes(previews)?;
        let metadata = CaptureSourceMetadata::new(&pixels, selected, cursor)?;
        let document = WireRasterDocument::from_capture_editor(
            editor,
            (source.bounds.width, source.bounds.height),
            metadata,
        )?;
        let raw = pixels.read_content(selected)?;
        let visible = ScreenshotPinPayload::new(source.bounds, selected, raw, style)?;
        Ok(Self {
            visible,
            pixels,
            document,
        })
    }
    pub(crate) fn into_parts(
        self,
    ) -> (
        crate::capture::Image,
        PinPresentation,
        Arc<CapturePixels>,
        WireRasterDocument,
    ) {
        let (raw, presentation) = self.visible.into_parts();
        (raw, presentation, self.pixels, self.document)
    }
    pub(crate) fn presentation(&self) -> PinPresentation {
        self.visible.presentation()
    }
}

#[cfg(test)]
#[path = "capture_pin_document_tests.rs"]
mod tests;
