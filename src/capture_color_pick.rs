//! 全屏冻结预览取色的资格与显示标签；不提交编辑命令，也不拥有像素。
use crate::{
    editor::Editor,
    geometry::{Rect, ScreenRect},
    pin_color_display::ColorDisplay,
    scene::SourcePixels,
};

#[derive(Clone, Copy)]
pub(crate) struct Availability {
    pub ordinary: bool,
    pub ready: bool,
    pub enabled: bool,
    pub preview_available: bool,
    pub active: bool,
    pub busy: bool,
    pub input_active: bool,
    pub menu_open: bool,
    pub replay: bool,
}

/// 只接受尚未确认选区的普通截图。宿主输入门之外，也核对真实编辑模型。
pub(crate) fn eligible(editor: &Editor, state: Availability) -> bool {
    state.ordinary
        && state.ready
        && state.enabled
        && state.preview_available
        && state.active
        && !state.busy
        && !state.input_active
        && !state.menu_open
        && !state.replay
        && editor.crop.is_none()
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Labels {
    pub color: String,
    pub coords: String,
}

pub(crate) struct Sample {
    pub pixels: crate::capture::Image,
    pub center: (u32, u32),
    pub rgb: u32,
}

/// 只读取一次旧 24×24 冻结邻域；颜色来自该裁片的实际中心，屏间空隙保留黑底。
pub(crate) fn sample(source: &SourcePixels<'_>, pointer: (i32, i32)) -> Option<Sample> {
    let canvas = source.canvas;
    let px = u32::try_from(pointer.0).ok()?;
    let py = u32::try_from(pointer.1).ok()?;
    if canvas.x != 0 || canvas.y != 0 || px >= canvas.width || py >= canvas.height {
        return None;
    }
    let width = 24.min(canvas.width);
    let height = 24.min(canvas.height);
    let x = px.saturating_sub(12).min(canvas.width - width);
    let y = py.saturating_sub(12).min(canvas.height - height);
    let center = (px - x, py - y);
    let pixels = source.read(Rect {
        x,
        y,
        width,
        height,
    });
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    if pixels.width != width || pixels.height != height || pixels.bgra.len() != expected {
        return None;
    }
    let index = usize::try_from(center.1)
        .ok()?
        .checked_mul(usize::try_from(width).ok()?)?
        .checked_add(usize::try_from(center.0).ok()?)?
        .checked_mul(4)?;
    let pixel = pixels.bgra.get(index..index.checked_add(4)?)?;
    let rgb = u32::from(pixel[2]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[0]);
    Some(Sample {
        pixels,
        center,
        rgb,
    })
}

/// local 与 preview 都来自本次冻结画布的物理像素，不使用逻辑像素或旧预览。
pub(crate) fn labels(
    display: &ColorDisplay,
    rgb: u32,
    local: (i32, i32),
    desktop: ScreenRect,
    preview: Option<Rect>,
) -> Option<Labels> {
    let x = u32::try_from(local.0).ok()?;
    let y = u32::try_from(local.1).ok()?;
    if x >= desktop.width || y >= desktop.height {
        return None;
    }
    let cursor = (
        desktop.left.checked_add(local.0)?,
        desktop.top.checked_add(local.1)?,
    );
    let origin = match preview {
        Some(rect) => {
            if !rect.valid(desktop.width, desktop.height) {
                return None;
            }
            // 负虚拟桌面允许 u32 范围的画布偏移；先相加，再核对屏幕 i32 范围。
            (
                i32::try_from(i64::from(desktop.left) + i64::from(rect.x)).ok()?,
                i32::try_from(i64::from(desktop.top) + i64::from(rect.y)).ok()?,
            )
        }
        None => (desktop.left, desktop.top),
    };
    let bgra = [rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8, 255];
    Some(Labels {
        color: display.format_color(bgra),
        coords: display.format_coords(cursor, origin),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Annotation, Point, Style, Tool};

    #[test]
    fn frozen_patch_centres_match_fixed_pixels_at_middle_corners_and_small_canvas() {
        let bytes: Vec<_> = (0..30)
            .flat_map(|y| (0..32).flat_map(move |x| [x as u8, y as u8, 66, 255]))
            .collect();
        let before = bytes.clone();
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(32, 30),
            planes: vec![crate::scene::PixelPlane {
                rect: Rect::full(32, 30),
                bgra: &bytes,
            }],
            overlay: None,
        };
        for (pointer, center, rgb, first, last, selected) in [
            (
                (16, 15),
                (12, 12),
                0x420F10,
                [4, 3, 66, 255],
                [27, 26, 66, 255],
                [16, 15, 66, 255],
            ),
            (
                (0, 0),
                (0, 0),
                0x420000,
                [0, 0, 66, 255],
                [23, 23, 66, 255],
                [0, 0, 66, 255],
            ),
            (
                (31, 0),
                (23, 0),
                0x42001F,
                [8, 0, 66, 255],
                [31, 23, 66, 255],
                [31, 0, 66, 255],
            ),
            (
                (0, 29),
                (0, 23),
                0x421D00,
                [0, 6, 66, 255],
                [23, 29, 66, 255],
                [0, 29, 66, 255],
            ),
            (
                (31, 29),
                (23, 23),
                0x421D1F,
                [8, 6, 66, 255],
                [31, 29, 66, 255],
                [31, 29, 66, 255],
            ),
        ] {
            let picked = sample(&source, pointer).unwrap();
            assert_eq!((picked.pixels.width, picked.pixels.height), (24, 24));
            assert_eq!(picked.center, center);
            assert_eq!(picked.rgb, rgb);
            assert_eq!(&picked.pixels.bgra[..4], &first);
            assert_eq!(&picked.pixels.bgra[picked.pixels.bgra.len() - 4..], &last);
            let index = ((center.1 * 24 + center.0) * 4) as usize;
            assert_eq!(&picked.pixels.bgra[index..index + 4], &selected);
        }
        assert_eq!(bytes, before);

        let small_bytes = [
            0, 0, 30, 255, 1, 0, 30, 255, 0, 1, 30, 255, 1, 1, 30, 255, 0, 2, 30, 255, 1, 2, 30,
            255,
        ];
        let small = SourcePixels {
            fill: None,
            canvas: Rect::full(2, 3),
            planes: vec![crate::scene::PixelPlane {
                rect: Rect::full(2, 3),
                bgra: &small_bytes,
            }],
            overlay: None,
        };
        let mut picked = sample(&small, (1, 2)).unwrap();
        assert_eq!((picked.pixels.width, picked.pixels.height), (2, 3));
        assert_eq!(picked.center, (1, 2));
        assert_eq!(picked.rgb, 0x1E0201);
        assert_eq!(&picked.pixels.bgra[20..24], &[1, 2, 30, 255]);
        picked.pixels.bgra.fill(0);
        assert_eq!(sample(&small, (1, 2)).unwrap().pixels.bgra, small_bytes);
    }

    #[test]
    fn frozen_patch_overlay_and_screen_gap_share_centre_and_reject_outside() {
        let left = [86, 52, 18, 255, 86, 52, 18, 255];
        let right = [205, 15, 171, 255, 205, 15, 171, 255];
        let cursor = [3, 2, 1, 255];
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(6, 1),
            planes: vec![
                crate::scene::PixelPlane {
                    rect: Rect::full(2, 1),
                    bgra: &left,
                },
                crate::scene::PixelPlane {
                    rect: Rect {
                        x: 4,
                        y: 0,
                        width: 2,
                        height: 1,
                    },
                    bgra: &right,
                },
            ],
            overlay: Some(crate::scene::PixelPlane {
                rect: Rect {
                    x: 1,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                bgra: &cursor,
            }),
        };
        for (point, center, rgb, selected) in [
            ((0, 0), (0, 0), 0x123456, [86, 52, 18, 255]),
            ((1, 0), (1, 0), 0x010203, [3, 2, 1, 255]),
            ((2, 0), (2, 0), 0, [0, 0, 0, 255]),
            ((3, 0), (3, 0), 0, [0, 0, 0, 255]),
            ((5, 0), (5, 0), 0xAB0FCD, [205, 15, 171, 255]),
        ] {
            let picked = sample(&source, point).unwrap();
            assert_eq!(picked.center, center);
            assert_eq!(picked.rgb, rgb);
            let index = (center.0 * 4) as usize;
            assert_eq!(&picked.pixels.bgra[index..index + 4], &selected);
        }
        for point in [(-1, 0), (6, 0), (0, -1), (0, 1)] {
            assert!(sample(&source, point).is_none());
        }
        assert_eq!(left, [86, 52, 18, 255, 86, 52, 18, 255]);
        assert_eq!(right, [205, 15, 171, 255, 205, 15, 171, 255]);
        assert_eq!(cursor, [3, 2, 1, 255]);
        let empty = SourcePixels {
            fill: None,
            canvas: Rect::full(0, 0),
            planes: Vec::new(),
            overlay: None,
        };
        assert!(sample(&empty, (0, 0)).is_none());
    }

    fn available() -> Availability {
        Availability {
            ordinary: true,
            ready: true,
            enabled: true,
            preview_available: true,
            active: true,
            busy: false,
            input_active: false,
            menu_open: false,
            replay: false,
        }
    }

    fn desktop() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: -1080,
            width: 3840,
            height: 2160,
        }
    }

    #[test]
    fn idle_preview_accepts_but_real_crop_drag_and_confirmed_crop_do_not() {
        let mut editor = Editor::new(Rect::full(3840, 2160), None);
        assert!(eligible(&editor, available()));
        editor.pointer_down((20, 30), 4.);
        assert!(editor.is_dragging());
        assert!(!eligible(&editor, available()));
        editor.pointer_up((120, 130));
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 100,
            })
        );
        assert!(!editor.is_dragging());
        assert!(!eligible(&editor, available()));
        editor.set_crop(None);
        assert!(eligible(&editor, available()));
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn real_pending_polyline_arrow_text_and_lasso_remain_ineligible() {
        let canvas = Rect::full(300, 200);
        for tool in [Tool::Polyline, Tool::Arrow] {
            let mut editor = Editor::new(canvas, Some(canvas));
            editor.select_tool(tool);
            editor.pointer_down((40, 40), 4.);
            editor.pointer_up((40, 40));
            assert!(editor.has_pending_drawing());
            assert!(!editor.is_dragging());
            // 即便宿主暂时清掉 crop，未提交的续绘也不能变成取色资格。
            editor.crop = None;
            assert!(!eligible(&editor, available()));
            assert!(editor.has_pending_drawing());
            assert!(!editor.document.can_undo());
        }

        let mut text = Editor::new(canvas, None);
        text.select_tool(Tool::Text);
        text.begin_text(None, Point::from_tuple((40, 40)));
        assert!(text.text_draft.is_some());
        assert!(!eligible(&text, available()));
        assert!(text.text_draft.is_some());
        assert!(!text.document.can_undo());

        let mut lasso = Editor::new(canvas, Some(canvas));
        lasso.select_tool(Tool::Lasso);
        lasso.pointer_down((40, 40), 4.);
        lasso.pointer_move((140, 40));
        lasso.pointer_move((140, 140));
        lasso.pointer_up((40, 140));
        assert!(lasso.lasso_draft.is_some());
        assert!(!lasso.is_dragging());
        lasso.crop = None;
        assert!(!eligible(&lasso, available()));
        assert!(lasso.lasso_draft.is_some());
        assert!(!lasso.document.can_undo());
    }

    #[test]
    fn host_exclusion_consumes_format_edges_without_delayed_toggle() {
        let editor = Editor::new(Rect::full(3840, 2160), None);
        let mut display = ColorDisplay::default();
        let rejected = [
            Availability {
                ordinary: false,
                ..available()
            },
            Availability {
                ready: false,
                ..available()
            },
            Availability {
                enabled: false,
                ..available()
            },
            Availability {
                preview_available: false,
                ..available()
            },
            Availability {
                active: false,
                ..available()
            },
            Availability {
                busy: true,
                ..available()
            },
            Availability {
                input_active: true,
                ..available()
            },
            Availability {
                menu_open: true,
                ..available()
            },
            Availability {
                replay: true,
                ..available()
            },
        ];
        for state in rejected {
            assert!(!eligible(&editor, state));
            assert!(!display.observe(true, true, eligible(&editor, state)));
            assert!(!display.observe(true, true, eligible(&editor, available())));
            let value = labels(&display, 0x123456, (141, 128), desktop(), None).unwrap();
            assert_eq!(value.color, "rgb(18, 52, 86)");
            assert_eq!(value.coords, "( -1779 , -952 )");
            assert!(!display.observe(false, false, true));
        }
        assert!(display.observe(true, true, eligible(&editor, available())));
        let value = labels(&display, 0x123456, (141, 128), desktop(), None).unwrap();
        assert_eq!(value.color, "#123456");
        assert_eq!(value.coords, "( 141 , 128 )");
    }

    #[test]
    fn labels_share_format_and_preview_origin_without_touching_editor_history() {
        let mut editor = Editor::new(Rect::full(3840, 2160), None);
        editor.document.insert(Annotation::new(
            Tool::Rectangle,
            Point::from_tuple((30, 40)),
            Style::default(),
        ));
        editor.document.undo();
        let revision = editor.document.revision();
        let edits = editor.document.edit_revision();
        let mut display = ColorDisplay::default();
        let preview = Some(Rect {
            x: 100,
            y: 80,
            width: 200,
            height: 160,
        });
        assert_eq!(
            labels(&display, 0x123456, (141, 128), desktop(), preview).unwrap(),
            Labels {
                color: "rgb(18, 52, 86)".into(),
                coords: "( -1779 , -952 )".into()
            }
        );
        assert!(display.observe(true, true, eligible(&editor, available())));
        assert_eq!(
            labels(&display, 0xAB0FCD, (141, 128), desktop(), preview).unwrap(),
            Labels {
                color: "#AB0FCD".into(),
                coords: "( 41 , 48 )".into()
            }
        );
        assert!(!display.observe(true, true, true));
        assert!(!display.observe(false, false, true));
        assert_eq!(
            labels(&display, 0xAB0FCD, (80, 60), desktop(), preview)
                .unwrap()
                .coords,
            "( -20 , -20 )"
        );
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edits);
        assert!(!editor.document.can_undo());
        assert!(editor.document.can_redo());
        assert!(editor.document.items().is_empty());
        editor.document.redo();
        assert_eq!(editor.document.items().len(), 1);
    }

    #[test]
    fn physical_coordinates_keep_negative_desktop_and_wide_relative_difference() {
        let mut display = ColorDisplay::default();
        let preview = Some(Rect {
            x: 100,
            y: 80,
            width: 200,
            height: 160,
        });
        assert_eq!(
            labels(&display, 0, (211, 329), desktop(), preview)
                .unwrap()
                .coords,
            "( -1709 , -751 )"
        );
        assert!(display.observe(false, true, true));
        assert_eq!(
            labels(&display, 0, (211, 329), desktop(), preview)
                .unwrap()
                .coords,
            "( 111 , 249 )"
        );
        assert_eq!(
            labels(&display, 0, (211, 329), desktop(), None)
                .unwrap()
                .coords,
            "( 211 , 329 )"
        );

        let wide = ScreenRect {
            left: i32::MIN,
            top: 0,
            width: u32::MAX,
            height: 1,
        };
        let far_preview = Rect {
            x: u32::MAX - 1,
            y: 0,
            width: 1,
            height: 1,
        };
        assert_eq!(
            labels(&display, 0, (0, 0), wide, Some(far_preview))
                .unwrap()
                .coords,
            "( -4294967294 , 0 )"
        );
    }

    #[test]
    fn out_of_canvas_invalid_preview_and_global_overflow_produce_no_labels() {
        let display = ColorDisplay::default();
        for local in [(-1, 0), (0, -1), (3840, 0), (0, 2160)] {
            assert!(labels(&display, 0x123456, local, desktop(), None).is_none());
        }
        assert!(labels(&display, 0x123456, (3839, 2159), desktop(), None).is_some());
        for preview in [
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            },
            Rect {
                x: 3839,
                y: 0,
                width: 2,
                height: 1,
            },
            Rect {
                x: u32::MAX,
                y: 0,
                width: 1,
                height: 1,
            },
        ] {
            assert!(labels(&display, 0x123456, (0, 0), desktop(), Some(preview)).is_none());
        }
        let overflow = ScreenRect {
            left: i32::MAX,
            top: i32::MAX,
            width: 3,
            height: 3,
        };
        assert!(labels(&display, 0, (1, 0), overflow, None).is_none());
        assert!(labels(&display, 0, (0, 1), overflow, None).is_none());
        assert!(labels(
            &display,
            0,
            (0, 0),
            overflow,
            Some(Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 1
            })
        )
        .is_none());
        assert!(labels(
            &display,
            0,
            (0, 0),
            ScreenRect {
                width: 0,
                ..desktop()
            },
            None
        )
        .is_none());
    }
}
