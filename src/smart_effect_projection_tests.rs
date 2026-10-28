use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    geometry::Rect,
    output_projection::OutputProjection,
    scene::{PixelPlane, Scene, SourcePixels},
};

#[test]
fn compound_effect_projection_preserves_sparse_gaps_and_physical_sampling() {
    let canvas = Rect::full(40, 24);
    let mut bytes = Vec::new();
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            bytes.extend_from_slice(&[
                ((x * 17 + y * 29) % 256) as u8,
                ((x * 43 + y * 7) % 256) as u8,
                ((x * 11 + y * 53) % 256) as u8,
                255,
            ]);
        }
    }
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &bytes,
        }],
        overlay: None,
    };
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut effect = Annotation::new(
            tool,
            Point { x: 6., y: 4. },
            Style {
                block_size: 5,
                opacity: 0.5,
                ..Default::default()
            },
        );
        effect
            .set_effect_regions(
                vec![
                    Bounds {
                        x: 6.,
                        y: 4.,
                        width: 6.,
                        height: 5.,
                    },
                    Bounds {
                        x: 9.,
                        y: 5.,
                        width: 7.,
                        height: 6.,
                    },
                    Bounds {
                        x: 24.,
                        y: 13.,
                        width: 3.,
                        height: 3.,
                    },
                ],
                true,
            )
            .unwrap();
        let scene = Scene::prepare(&[effect], &source);
        assert!(scene.errors.is_empty(), "{:?}", scene.errors);
        let mut original = source.read(canvas);
        scene.apply(&mut original, canvas);
        let gap = (10 * canvas.width as usize + 19) * 4;
        assert_eq!(&original.bgra[gap..gap + 4], &bytes[gap..gap + 4]);
        for zoom in [1., 1.5, 2.75] {
            let roi = Rect {
                x: 4,
                y: 2,
                width: 26,
                height: 18,
            };
            let projection = OutputProjection::new(roi, zoom).unwrap();
            let mut projected = projection.background(&source).unwrap();
            scene
                .apply_projected(&mut projected, &source, &projection)
                .unwrap();
            for y in 0..projected.height {
                for x in 0..projected.width {
                    let p = projection.source_center(x, y);
                    let src =
                        (p.y.floor() as usize * canvas.width as usize + p.x.floor() as usize) * 4;
                    let dst = (y as usize * projected.width as usize + x as usize) * 4;
                    assert_eq!(
                        &projected.bgra[dst..dst + 4],
                        &original.bgra[src..src + 4],
                        "{tool:?} zoom={zoom} at ({x},{y})"
                    );
                }
            }
        }
    }
}
