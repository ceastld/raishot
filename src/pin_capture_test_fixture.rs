//! 仅合成CPU样本，供真实handoff/内部资源/关闭/工作区模型共用。
use super::*;
use crate::{
    annotation::{Annotation, HighlighterBlend, Point, Style, Tool},
    canvas_source::CanvasSource,
    crop_mask::Polygon,
    editor::Editor,
    pin_raster_document::WireRasterDocument,
    scene::Scene,
    selection_finish::SelectionFinishStyle,
};
pub(crate) struct Case {
    pub(crate) pixels: Arc<CapturePixels>,
    pub(crate) source: CanvasSource,
    pub(crate) previews: Vec<Arc<RenderImage>>,
    pub(crate) editor: Editor,
    pub(crate) selected: Rect,
    pub(crate) cursor: CapturedCursor,
    pub(crate) style: SelectionFinishStyle,
}
impl Case {
    pub(crate) fn new() -> Self {
        let bounds = ScreenRect {
            left: -1921,
            top: -701,
            width: 96,
            height: 72,
        };
        let full = Rect::full(bounds.width, bounds.height);
        let mut bytes = Vec::new();
        for y in 0..bounds.height {
            for x in 0..bounds.width {
                bytes.extend_from_slice(&[
                    (x * 13 + y * 7) as u8,
                    (x * 3 + y * 17) as u8,
                    (x * 19 + y * 5) as u8,
                    255,
                ]);
            }
        }
        let frame = crate::canvas_source::render_image(bounds.width, bounds.height, bytes);
        let pixels = Arc::new(
            CapturePixels::new(
                bounds,
                None,
                vec![OwnedPlane {
                    rect: full,
                    image: frame,
                }],
            )
            .unwrap(),
        );
        let host = Rect {
            x: 15,
            y: 11,
            width: 52,
            height: 46,
        };
        let mut editor = Editor::new(full, Some(host));
        let make = |tool, from, to, style| {
            let mut annotation = Annotation::new(tool, from, style);
            annotation.update(to);
            annotation
        };
        let rectangle = editor.document.insert(make(
            Tool::Rectangle,
            Point { x: 8., y: 7. },
            Point { x: 74., y: 62. },
            Style {
                filled: true,
                color: 0x4e98c2,
                opacity: 0.42,
                ..Default::default()
            },
        ));
        let multiply = editor.document.insert(make(
            Tool::HighlighterRect,
            Point { x: 10.3, y: 16.5 },
            Point { x: 79.1, y: 31.2 },
            Style {
                color: 0xfee561,
                highlighter_blend: HighlighterBlend::Multiply,
                ..Default::default()
            },
        ));
        let mosaic = editor.document.insert(make(
            Tool::Mosaic,
            Point { x: 8., y: 9. },
            Point { x: 75., y: 49. },
            Style {
                block_size: 7,
                ..Default::default()
            },
        ));
        editor.document.insert(make(
            Tool::Blur,
            Point { x: 11., y: 25. },
            Point { x: 71., y: 62. },
            Style {
                block_size: 4,
                ..Default::default()
            },
        ));
        assert!(editor.document.set_group(&[rectangle, multiply], true));
        assert!(editor
            .document
            .set_group(&[rectangle, multiply, mosaic], true));
        editor.document.set_crop_mask(Some(
            Polygon::new(&[
                Point { x: 19., y: 14. },
                Point { x: 63., y: 17. },
                Point { x: 55., y: 52. },
                Point { x: 23., y: 48. },
            ])
            .unwrap(),
        ));
        let mut changed = editor.document.get(rectangle).unwrap().clone();
        changed.style.color = 0xe43971;
        editor.document.replace(rectangle, changed);
        editor.document.undo();
        assert!(editor.document.can_undo() && editor.document.can_redo());
        let selected =
            crate::capture_pin_output::output_rect(full, host, editor.document.crop_mask.as_ref())
                .unwrap();
        let (source, previews) = CanvasSource::capture_context(pixels.clone(), selected).unwrap();
        let cursor = CapturedCursor {
            rect: Rect {
                x: 31,
                y: 29,
                width: 2,
                height: 3,
            },
            image: Image {
                width: 2,
                height: 3,
                bgra: [9, 14, 241, 255].repeat(6),
                readback_ms: 0.,
            },
        };
        let style = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 6.25,
            border_enabled: true,
            border_thickness: 1.75,
            border_color: 0xff31_d6ba,
            shadow_enabled: true,
            shadow_blur: 3.25,
            ..Default::default()
        };
        Self {
            pixels,
            source,
            previews,
            editor,
            selected,
            cursor,
            style,
        }
    }
    pub(crate) fn metadata(&self) -> CaptureSourceMetadata {
        CaptureSourceMetadata::new(&self.pixels, self.selected, Some(&self.cursor)).unwrap()
    }
    pub(crate) fn document(&self) -> WireRasterDocument {
        WireRasterDocument::from_capture_editor(&self.editor, (96, 72), self.metadata()).unwrap()
    }
    pub(crate) fn raw(&self) -> Image {
        self.pixels.read_content(self.selected).unwrap()
    }
    pub(crate) fn scene(&self, editor: &Editor) -> Scene {
        let mut source = self.pixels.source_pixels();
        source.overlay = Some(self.cursor.plane());
        Scene::prepare(&editor.document_annotations(), &source)
    }
    pub(crate) fn output(&self, editor: &Editor) -> Image {
        let scene = self.scene(editor);
        assert!(scene.errors.is_empty(), "{:?}", scene.errors);
        crate::capture_pin_output::content_image(
            &self.source,
            &scene,
            self.selected,
            Some(&self.cursor),
            editor.document.crop_mask.as_ref(),
        )
        .unwrap()
    }
    pub(crate) fn editor_from(&self, document: &WireRasterDocument) -> Editor {
        let mut editor = Editor::new(self.source.full(), Some(self.selected));
        document.restore_into(&mut editor).unwrap();
        editor
    }
}
pub(crate) struct TestDir(pub(crate) std::path::PathBuf);
impl TestDir {
    pub(crate) fn new(purpose: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(format!(
                "capture-source-{purpose}-{}-{n}",
                std::process::id()
            ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}
