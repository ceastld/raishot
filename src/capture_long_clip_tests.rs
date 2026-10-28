use super::*;
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    long_clip::LongClip,
    scene::{PixelPlane, SourcePixels},
    stitch::ScrollAxis,
};

#[test]
fn long_clip_output_renders_scene_and_lasso_before_composing_both_axes() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let image = Image {
            width: 24,
            height: 30,
            bgra: [11, 22, 33, 255].repeat(24 * 30),
            readback_ms: 0.,
        };
        let (source, previews) = CanvasSource::editor_image(image).unwrap();
        let pixels = SourcePixels {
            canvas: source.full(),
            fill: None,
            planes: vec![PixelPlane {
                rect: source.original_rect(0),
                bgra: source.original(&previews[0]).as_bytes(0).unwrap(),
            }],
            overlay: None,
        };
        let mut mark = Annotation::new(
            Tool::Rectangle,
            Point { x: 2., y: 3. },
            Style {
                color: 0xf02040,
                filled: true,
                ..Default::default()
            },
        );
        mark.update(Point { x: 22., y: 27. });
        let scene = Scene::prepare(&[mark], &pixels);
        let mask = Polygon::new(&[
            Point { x: 5., y: 5. },
            Point { x: 20., y: 5. },
            Point { x: 20., y: 25. },
            Point { x: 5., y: 25. },
        ])
        .unwrap();
        let mut clip = LongClip::new(24, 30, axis).unwrap();
        clip.delete(9..17).unwrap();
        clip.set_cross(1..22).unwrap();
        let original = content_image(&source, &scene, source.full(), None, Some(&mask)).unwrap();
        let expected = crate::long_clip_export::compose(&original, &clip).unwrap();
        let actual = clipped_content_image(&source, &scene, &clip, Some(&mask)).unwrap();
        assert_eq!((actual.width, actual.height), clip.output_size());
        assert_eq!(actual.bgra, expected.bgra);
        assert!(actual.bgra.chunks_exact(4).any(|p| p[3] == 0));
        assert!(actual
            .bgra
            .chunks_exact(4)
            .any(|p| p[3] == 255 && p[2] > 200));
        // 输出不能改写用于恢复删段、Scene采样和后续标注的原图。
        assert_eq!(
            source.read_region(source.full()).unwrap().bgra,
            [11, 22, 33, 255].repeat(24 * 30)
        );
    }
}

#[test]
fn long_clip_output_rejects_mismatched_document_source() {
    let (source, _) = CanvasSource::editor_image(Image {
        width: 3,
        height: 8,
        bgra: vec![255; 3 * 8 * 4],
        readback_ms: 0.,
    })
    .unwrap();
    let clip = LongClip::new(3, 9, ScrollAxis::Vertical).unwrap();
    assert!(clipped_content_image(&source, &Scene::default(), &clip, None).is_err());
}
