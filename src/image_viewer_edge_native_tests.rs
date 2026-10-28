//! 自有小PNG的完整图放大采样检查；只读回自有窗口的3×3 framebuffer。
use super::preview_native_tests::{capture_pixels, tick, wait_view, ImageWindow, Lifecycle};
use super::*;
use gpui::AsyncApp;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

const ENCODED_LIMIT: usize = 64 * 1024;
const INITIAL: [[u8; 4]; 2] = [[38, 62, 70, 255]; 2];
const ONE: [[u8; 4]; 1] = [[74, 42, 58, 255]];
const BANDS: [[u8; 4]; 4] = [
    [28, 36, 72, 255],
    [88, 52, 24, 255],
    [32, 84, 60, 255],
    [76, 28, 92, 255],
];

fn require(condition: bool, message: &str) -> crate::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

struct LimitedFile {
    file: File,
    bytes: usize,
}

impl Write for LimitedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > ENCODED_LIMIT.saturating_sub(self.bytes) {
            return Err(io::Error::other("Edge PNG fixture exceeds 64 KiB"));
        }
        let count = self.file.write(bytes)?;
        self.bytes += count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub(super) struct Fixtures {
    pub(super) initial: PathBuf,
    one: PathBuf,
    bands: PathBuf,
}

pub(super) fn prepare(root: &Path) -> crate::Result<Fixtures> {
    let fixtures = Fixtures {
        initial: root.join("edge-two.png"),
        one: root.join("edge-one.png"),
        bands: root.join("edge-bands.png"),
    };
    for (path, colors) in [
        (&fixtures.initial, INITIAL.as_slice()),
        (&fixtures.one, ONE.as_slice()),
        (&fixtures.bands, BANDS.as_slice()),
    ] {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut output = LimitedFile { file, bytes: 0 };
        let mut encoder = png::Encoder::new(&mut output, colors.len() as u32, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&colors.iter().flatten().copied().collect::<Vec<_>>())?;
        writer.finish()?;
    }
    Ok(fixtures)
}

fn verify_full(
    view: &ImageViewer,
    colors: &[[u8; 4]],
    previous: Option<u64>,
) -> crate::Result<u64> {
    let image = view.image.as_ref().ok_or("Edge Full image is missing")?;
    require(
        view.error.is_none()
            && image.has_full_pixels()
            && image.preview_source.is_none()
            && view.preview_viewport.is_none()
            && view.preview_region.surface().is_none(),
        "Edge fixture retained an error, Preview or ROI",
    )?;
    require(
        previous != Some(view.serial),
        "Edge replacement did not change serial",
    )?;
    let raw = image
        .raw_page
        .as_ref()
        .ok_or("Edge Full has no CPU pixels")?;
    let expected: Vec<u8> = colors
        .iter()
        .flat_map(|&[r, g, b, a]| [b, g, r, a])
        .collect();
    require(
        (raw.width, raw.height) == (colors.len() as u32, 1) && raw.bgra == expected,
        "Edge Full CPU bytes differ from the private PNG",
    )?;
    require(
        image.tiles.len() == 1 && image.tiles[0].0 == Rect::full(colors.len() as u32, 1),
        "Edge fixture must use one complete Full image quad",
    )?;
    Ok(view.serial)
}

fn bgra([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [b, g, r, a]
}

// 独立按源像素中心在编码字节上插值；不复用atlas UV或shader钳制实现。
// 不透明输入及BGRA8_UNORM atlas/目标使此处无需gamma或alpha换算。
fn linear_bgra(colors: &[[u8; 4]], source_x: f64) -> [u8; 4] {
    let u = source_x - 0.5;
    let base = u.floor();
    let weight = u - base;
    let left = colors[(base as isize).clamp(0, colors.len() as isize - 1) as usize];
    let right = colors[(base as isize + 1).clamp(0, colors.len() as isize - 1) as usize];
    bgra(std::array::from_fn(|channel| {
        (f64::from(left[channel]) * (1. - weight) + f64::from(right[channel]) * weight).round()
            as u8
    }))
}

async fn sample(
    window: ImageWindow,
    viewport: Viewport,
    area: Rect,
    colors: &[[u8; 4]],
    rect: [u32; 4],
    linear: bool,
    phase: &str,
    cx: &mut AsyncApp,
) -> crate::Result<()> {
    let left = f64::from(viewport.offset.x);
    let top = f64::from(viewport.offset.y);
    let zoom = f64::from(viewport.zoom);
    let right = left + colors.len() as f64 * zoom;
    let bottom = top + zoom;
    let mut expected = Vec::with_capacity(36);
    let mut sources = Vec::with_capacity(9);
    let mut differs_from_nearest = false;
    for y in rect[1]..rect[1] + 3 {
        for x in rect[0]..rect[0] + 3 {
            let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let source = ((px - left) / zoom, (py - top) / zoom);
            require(
                px - left >= 1. && right - px >= 1. && py - top >= 1. && bottom - py >= 1.
                    && x >= area.x && y >= area.y
                    && x < area.x + area.width && y < area.y + area.height,
                &format!("{phase}: sample crosses full quad coverage/area: rect={rect:?}, source={source:?}"),
            )?;
            let pixel = if linear {
                require(
                    source.0 > 0.5 && source.0 < colors.len() as f64 - 0.5,
                    "Interior blend must have two source texel centers",
                )?;
                let blended = linear_bgra(colors, source.0);
                let nearest = bgra(colors[(source.0.floor() as usize).min(colors.len() - 1)]);
                differs_from_nearest |=
                    blended.iter().zip(nearest).any(|(a, b)| a.abs_diff(b) >= 4);
                blended
            } else {
                require(
                    source.0 < 0.5
                        || source.0 > colors.len() as f64 - 0.5
                        || source.1 < 0.5
                        || source.1 > 0.5,
                    "Edge sample missed the outer half-texel footprint",
                )?;
                bgra(colors[0])
            };
            sources.push(source);
            expected.extend_from_slice(&pixel);
        }
    }
    require(
        !linear || differs_from_nearest,
        "Linear oracle cannot distinguish nearest filtering",
    )?;
    let actual = capture_pixels(window, rect, phase, cx).await?;
    for (index, (actual, expected)) in actual
        .chunks_exact(4)
        .zip(expected.chunks_exact(4))
        .enumerate()
    {
        require(actual.iter().zip(expected).all(|(a, b)| a.abs_diff(*b) <= 1),
            &format!("{phase}: rect={rect:?}, source={:?}, actual={actual:?}, expected={expected:?}, tolerance=1", sources[index]))?;
    }
    eprintln!("edge native phase={phase} rect={rect:?} source={sources:?} actual_bgra={actual:?} expected_bgra={expected:?} tolerance=1");
    Ok(())
}

pub(super) async fn exercise(
    window: ImageWindow,
    fixtures: &Fixtures,
    lifecycle: &Lifecycle,
    cx: &mut AsyncApp,
) -> crate::Result<()> {
    let mut previous = None;
    for (path, colors, linear) in [
        (&fixtures.initial, INITIAL.as_slice(), false),
        (&fixtures.one, ONE.as_slice(), false),
        (&fixtures.bands, BANDS.as_slice(), true),
    ] {
        if previous.is_some() {
            window.update(cx, |view, _, cx| view.open_path(path.clone(), false, cx))?;
        }
        let serial = wait_view(window, "edge Full loaded", cx, |view, _, _| {
            if view.loading {
                return Ok(None);
            }
            Ok(Some(verify_full(view, colors, previous)?))
        })
        .await?;
        if previous.is_some() {
            wait_view(window, "edge old textures released", cx, |_, window, _| {
                window.refresh();
                Ok((!lifecycle.old_pixels_pending()).then_some(()))
            })
            .await?;
        }
        window.update(cx, |view, _, _| -> crate::Result<()> {
            let image = view
                .image
                .as_ref()
                .ok_or("Edge image disappeared before observation")?;
            for (_, texture) in &image.tiles {
                lifecycle.remember_image(texture);
            }
            Ok(())
        })??;
        for fraction in [(0., 0.), (0.25, 0.375)] {
            let (viewport, area) =
                wait_view(window, "edge physical placement", cx, |view, window, cx| {
                    require(
                        view.serial == serial && !view.loading,
                        "Edge source changed during placement",
                    )?;
                    let area = image_area(window);
                    require(
                        area.width >= 96 && area.height >= 96,
                        "Edge image area is too small",
                    )?;
                    view.zoom(8., area_center(window));
                    view.auto_fit = false;
                    view.viewport.offset = Point {
                        x: (area.x + 32) as f32 + fraction.0,
                        y: (area.y + 32) as f32 + fraction.1,
                    };
                    cx.notify();
                    window.refresh();
                    Ok(Some((view.viewport, image_area(window))))
                })
                .await?;
            require(
                viewport.zoom == 8.,
                "Edge fixture did not retain real viewer zoom 8",
            )?;
            tick(cx).await;
            let x = viewport.offset.x.floor() as u32;
            let y = viewport.offset.y.floor() as u32;
            let phase = format!("{}x1 offset={fraction:?}", colors.len());
            if linear {
                for boundary in 1..colors.len() as u32 {
                    let rect = [x + boundary * 8 - 1, y + 1, 3, 3];
                    let phase = format!("{phase} blend={boundary}");
                    sample(window, viewport, area, colors, rect, true, &phase, cx).await?;
                }
            } else {
                let right = x + colors.len() as u32 * 8;
                for (edge, sx, sy) in [
                    ("left", x + 1, y + 2),
                    ("top", x + 2, y + 1),
                    ("right", right - 4, y + 2),
                    ("bottom", x + 2, y + 4),
                ] {
                    let rect = [sx, sy, 3, 3];
                    let phase = format!("{phase} {edge}");
                    sample(window, viewport, area, colors, rect, false, &phase, cx).await?;
                }
            }
        }
        previous = Some(serial);
    }
    Ok(())
}

#[test]
#[ignore = "显式单独运行自有小PNG完整图放大边缘/图内线性混色及窗口退出检查"]
fn image_viewer_edge_native_linear_clamps_edges_and_keeps_interior_blend() {
    super::preview_native_tests::run_edge_window()
        .expect("Full image edge native window check failed");
}
