//! 长贴图的非均色显示验收；只读自有 GPU 帧，不读桌面或剪贴板。
use super::{close, ensure, presented, ready, tick, Report, WAIT};
use crate::{
    annotation::Point,
    capture::Image,
    geometry::Rect,
    gpui_capture::hwnd,
    pin_model::{PinState, Placement},
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{AsyncApp, WindowHandle};
use std::{sync::Arc, time::Instant};

const SHORT: u32 = 256;
const LONG: u32 = 32_000;
const STRIP: u32 = 17;
const BLOCKS: [[u8; 4]; 3] = [
    [40, 100, 180, 255],
    [180, 60, 100, 255],
    [100, 180, 40, 255],
];

fn fixture_pixel(x: u32, y: u32) -> [u8; 4] {
    match x / 64 {
        0 => BLOCKS[(y / 64 % 3) as usize],
        1 => {
            if y.is_multiple_of(2) {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            }
        }
        2 => [231, 17, 199, 0], // 隐藏 RGB 不得渗入透明棋盘。
        _ => [20, 80, 180, 128],
    }
}
fn fixture() -> Image {
    let mut bgra = Vec::with_capacity((SHORT * LONG * 4) as usize);
    for y in 0..LONG {
        for x in 0..SHORT {
            bgra.extend_from_slice(&fixture_pixel(x, y));
        }
    }
    Image {
        width: SHORT,
        height: LONG,
        bgra,
        readback_ms: 0.,
    }
}
fn raw_pixel(x: u32, y: u32, horizontal: bool) -> [u8; 4] {
    // Rotate(1) 将 (sx,sy) 映射为 (LONG-1-sy,sx)。Oracle 不读生产预览。
    if horizontal {
        fixture_pixel(y, LONG - 1 - x)
    } else {
        fixture_pixel(x, y)
    }
}
fn matte_pixel(x: u32, y: u32, horizontal: bool) -> [u8; 4] {
    let pixel = raw_pixel(x, y, horizontal);
    let checker = if (x / 16 + y / 16).is_multiple_of(2) {
        224
    } else {
        192
    };
    let alpha = u32::from(pixel[3]);
    let mut output = [0; 4];
    for c in 0..3 {
        output[c] = ((u32::from(pixel[c]) * alpha + checker * (255 - alpha) + 127) / 255) as u8;
    }
    output[3] = 255;
    output
}

#[derive(Clone, Copy)]
struct View {
    placement: Placement,
    image: (u32, u32),
    client: (u32, u32),
    dpi: f32,
}
impl View {
    fn projection(self) -> crate::viewport::Viewport {
        crate::pin_zoom::projection(self.image, self.client)
    }
}
fn view(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<View> {
    pin.update(cx, |v, w, _| -> Result<View> {
        let viewport = w.viewport_size();
        let dpi = w.scale_factor();
        Ok(View {
            placement: crate::pin_native::rect(hwnd(w))?,
            image: v.image_size,
            client: (
                (f32::from(viewport.width) * dpi).round() as u32,
                (f32::from(viewport.height) * dpi).round() as u32,
            ),
            dpi,
        })
    })?
}
async fn capture(pin: WindowHandle<PinView>, rect: [u32; 4], cx: &mut AsyncApp) -> Result<Vec<u8>> {
    pin.update(cx, |_, w, _| w.request_frame_capture(rect))??;
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        if let Some(value) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
            let bytes = value.map_err(|error| error.to_string())?;
            ensure(
                bytes.len() == (rect[2] * rect[3] * 4) as usize,
                "非均色 GPU 抽样长度错误",
            )?;
            return Ok(bytes);
        }
        ensure(Instant::now() < end, "非均色自有 GPU 帧读回超时")?;
    }
}
fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(&a, &b)| a.abs_diff(b))
        .max()
        .unwrap_or(0)
}
fn visible(rect: [u32; 4], state: View, work: Placement, horizontal: bool) -> bool {
    let x = i64::from(state.placement.x) + i64::from(rect[0]);
    let y = i64::from(state.placement.y) + i64::from(rect[1]);
    // 为下一次沿长轴 +1px 移动预留一像素；双方都在真实屏幕工作区内。
    x >= i64::from(work.x)
        && y >= i64::from(work.y)
        && x + i64::from(rect[2]) + (if horizontal { 1 } else { 0 })
            <= i64::from(work.x) + i64::from(work.width)
        && y + i64::from(rect[3]) + (if horizontal { 0 } else { 1 })
            <= i64::from(work.y) + i64::from(work.height)
        && rect[0] + rect[2] <= state.client.0
        && rect[1] + rect[3] <= state.client.1
}
fn strip(state: View, source: Point, horizontal: bool) -> Result<[u32; 4]> {
    let point = state.projection().window_point(source);
    let x = point.x.floor().max(0.) as u32;
    let y = point.y.floor().max(0.) as u32;
    let half = STRIP / 2;
    if horizontal {
        ensure(x >= half, "横向抽样位于窗口边界")?;
        Ok([x - half, y, STRIP, 1])
    } else {
        ensure(y >= half, "纵向抽样位于窗口边界")?;
        Ok([x, y - half, 1, STRIP])
    }
}

/// 独立连续面积 oracle；只算抽样条带，不创建第二张完整长图。
/// .375 且 contain offset 为 0 时，目标像素格与整张源图固定比例对应。
fn box_oracle(rect: [u32; 4], state: View, horizontal: bool) -> Vec<u8> {
    let zoom = (f64::from(state.client.0) / f64::from(state.image.0))
        .min(f64::from(state.client.1) / f64::from(state.image.1));
    let ox = (f64::from(state.client.0) - f64::from(state.image.0) * zoom) / 2.;
    let oy = (f64::from(state.client.1) - f64::from(state.image.1) * zoom) / 2.;
    let mut output = Vec::with_capacity((rect[2] * rect[3] * 4) as usize);
    for y in rect[1]..rect[1] + rect[3] {
        for x in rect[0]..rect[0] + rect[2] {
            let x0 = (f64::from(x) - ox) / zoom;
            let y0 = (f64::from(y) - oy) / zoom;
            let x1 = (f64::from(x + 1) - ox) / zoom;
            let y1 = (f64::from(y + 1) - oy) / zoom;
            let mut sums = [0.; 3];
            for sy in y0.floor().max(0.) as u32..(y1.ceil() as u32).min(state.image.1) {
                for sx in x0.floor().max(0.) as u32..(x1.ceil() as u32).min(state.image.0) {
                    let weight = (x1.min(f64::from(sx + 1)) - x0.max(f64::from(sx)))
                        * (y1.min(f64::from(sy + 1)) - y0.max(f64::from(sy)));
                    let pixel = matte_pixel(sx, sy, horizontal);
                    for c in 0..3 {
                        sums[c] += f64::from(pixel[c]) * weight;
                    }
                }
            }
            let area = (x1 - x0) * (y1 - y0);
            output.extend_from_slice(&[
                (sums[0] / area).round() as u8,
                (sums[1] / area).round() as u8,
                (sums[2] / area).round() as u8,
                255,
            ]);
        }
    }
    output
}
fn raw_samples(pin: WindowHandle<PinView>, horizontal: bool, cx: &mut AsyncApp) -> Result<usize> {
    pin.update(cx, |v, _, cx| -> Result<usize> {
        let canvas = v.canvas.as_ref().ok_or("非均色长图没有标注画布")?.read(cx);
        let mut count = 0;
        for y in [4095, 4096, 16383, 16384, LONG - 1] {
            for x in [32, 96, 160, 224] {
                let (tx, ty) = if horizontal {
                    (LONG - 1 - y, x)
                } else {
                    (x, y)
                };
                let image = canvas.source.read_region(Rect {
                    x: tx,
                    y: ty,
                    width: 1,
                    height: 1,
                })?;
                ensure(
                    image.bgra == fixture_pixel(x, y),
                    "ROI 显示后原始跨界像素/透明度改变",
                )?;
                count += 1;
            }
        }
        Ok(count)
    })?
}
async fn place_feature(
    pin: WindowHandle<PinView>,
    source: Point,
    horizontal: bool,
    work: Placement,
    cx: &mut AsyncApp,
) -> Result<()> {
    let state = view(pin, cx)?;
    let local = state.projection().window_point(source);
    let mut next = state.placement;
    if horizontal {
        next.x = (i64::from(work.x) + i64::from(work.width / 3) - local.x.round() as i64) as i32;
        next.y = work.y.saturating_add((work.height / 3) as i32);
    } else {
        next.x = work.x.saturating_add((work.width / 3) as i32);
        next.y = (i64::from(work.y) + i64::from(work.height / 3) - local.y.round() as i64) as i32;
    }
    pin.update(cx, |v, w, cx| v.request_placement(next, w, cx))?;
    ready(pin, cx).await
}
async fn sample_stage(
    report: &mut Report,
    pin: WindowHandle<PinView>,
    name: &str,
    horizontal: bool,
    source_long: Option<u32>,
    work: Placement,
    cx: &mut AsyncApp,
) -> Result<()> {
    let before = view(pin, cx)?;
    let projection = before.projection();
    let mut strips = Vec::new();
    for band in 0..if source_long.is_some() { 4 } else { 1 } {
        let source = if let Some(long) = source_long {
            let cross = (band * 64 + 32) as f32;
            if horizontal {
                Point {
                    x: long as f32,
                    y: cross,
                }
            } else {
                Point {
                    x: cross,
                    y: long as f32,
                }
            }
        } else {
            Point {
                x: before.image.0 as f32 / 2.,
                y: before.image.1 as f32 / 2.,
            }
        };
        let rect = strip(before, source, horizontal)?;
        ensure(
            visible(rect, before, work, horizontal),
            "非均色抽样不在两次都可见的自有窗口范围",
        )?;
        strips.push((band, rect, capture(pin, rect, cx).await?));
    }
    // 第一组 .375 独立 oracle 覆盖条纹和透明棋盘；最大缩放存在半像素 contain 留白，
    // 这里只验证宽色块和移动一致性，不把混合采样误称为精确原图像素证明。
    let absolute = source_long.is_some()
        && (projection.zoom - 0.375).abs() < 0.000_001
        && projection.offset.x.abs() < 0.000_001
        && projection.offset.y.abs() < 0.000_001;
    let mut moved = before.placement;
    if horizontal {
        moved.x = moved.x.saturating_add(1);
    } else {
        moved.y = moved.y.saturating_add(1);
    }
    pin.update(cx, |v, w, cx| v.request_placement(moved, w, cx))?;
    ready(pin, cx).await?;
    presented(pin, cx).await?;
    let after = view(pin, cx)?;
    ensure(
        after.client == before.client && after.image == before.image,
        "单像素移动意外改变投影尺寸",
    )?;
    ensure(after.placement == moved, "原生窗口未实际完成单像素移动")?;
    let mut checks = Vec::new();
    let mut okay = true;
    for (band, rect, baseline) in strips {
        ensure(
            visible(rect, after, work, horizontal),
            "移动后抽样离开屏幕可见范围",
        )?;
        let actual = capture(pin, rect, cx).await?;
        let movement_diff = max_diff(&baseline, &actual);
        let expected = if absolute || source_long.is_some() && band == 0 {
            Some(box_oracle(rect, before, horizontal))
        } else {
            None
        };
        let oracle_diff = expected
            .as_ref()
            .map(|expected| max_diff(&baseline, expected));
        let passed = movement_diff <= 1 && oracle_diff.is_none_or(|diff| diff <= 3);
        okay &= passed;
        checks.push(serde_json::json!({
            "band":band,"rect":rect,"baseline":baseline,"moved":actual,
            "movement_max_diff":movement_diff,"movement_tolerance":1,
            "independent_box_expected":expected,"oracle_max_diff":oracle_diff,
            "oracle_tolerance":3,"ok":passed,
        }));
    }
    report.record(
        "nonuniform-projection-quality",
        serde_json::json!({
            "orientation":if horizontal {"landscape"} else {"portrait"},"mode":name,"ok":okay,
            "source_long":source_long,"client":before.client,"source_dimensions":before.image,
            "dpi":before.dpi,"actual_contain_zoom":projection.zoom,
            "contain_offset":[projection.offset.x,projection.offset.y],
            "before_origin":[before.placement.x,before.placement.y],
            "after_origin":[after.placement.x,after.placement.y],
            "source":"owned-gpui-framebuffer","all_bands_absolute_oracle":absolute,"checks":checks,
        }),
    )?;
    ensure(okay, &format!("非均色 {name} 长图显示像素/移动相位错误"))
}

pub(super) async fn verify(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let work = crate::pin_native::work_area((100, 100));
    ensure(
        work.width >= 240 && work.height >= 240,
        "显示工作区不足以执行非均色专项",
    )?;
    for horizontal in [false, true] {
        let pin =
            cx.update(|cx| PinView::open_state(fixture(), (100, 100), PinState::default(), cx))??;
        let original = pin.update(cx, |v, w, _| {
            crate::gpui_smoke::isolate_input(hwnd(w))?;
            Ok::<_, Box<dyn std::error::Error>>(Arc::downgrade(&v.original))
        })??;
        ready(pin, cx).await?;
        if horizontal {
            pin.update(cx, |v, w, cx| v.perform(PinAction::Rotate(1), w, cx))?;
            ready(pin, cx).await?;
            pin.update(cx, |v, w, cx| v.perform(PinAction::Fit, w, cx))?;
            ready(pin, cx).await?;
        }
        let current = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
        sample_stage(
            report,
            pin,
            "fit-baseline-and-one-pixel-move",
            horizontal,
            None,
            work,
            cx,
        )
        .await?;
        for (name, requested_zoom, source_y) in [
            ("fractional-0.375-around-4096", 0.375, 4096 + 32),
            ("maximum-around-16384", 1., 16384 + 32),
        ] {
            pin.update(cx, |v, w, cx| {
                v.perform(PinAction::Zoom(requested_zoom), w, cx)
            })?;
            ready(pin, cx).await?;
            let source_long = if horizontal {
                LONG - 1 - source_y
            } else {
                source_y
            };
            let source = if horizontal {
                Point {
                    x: source_long as f32,
                    y: 128.,
                }
            } else {
                Point {
                    x: 128.,
                    y: source_long as f32,
                }
            };
            place_feature(pin, source, horizontal, work, cx).await?;
            sample_stage(report, pin, name, horizontal, Some(source_long), work, cx).await?;
        }
        let count = raw_samples(pin, horizontal, cx)?;
        close(pin, original, cx).await?;
        ensure(
            current.upgrade().is_none(),
            "非均色贴图关闭后当前原始像素 Arc 未释放",
        )?;
        report.record(
            "nonuniform-projection-closed",
            serde_json::json!({
                "orientation":if horizontal {"landscape"} else {"portrait"},
                "raw_cross_boundary_samples":count,"raw_alpha_unchanged":true,
                "original_weak_expired":true,"current_weak_expired":true,
            }),
        )?;
    }
    report.groups.push(serde_json::json!({
        "name":"nonuniform-long-pin-projection-quality","ok":true,
        "source_dimensions":[SHORT,LONG],"orientations":2,"projection_states_per_orientation":3,
        "patterns":["opaque-64px-blocks","one-pixel-alternating-stripes","transparent-hidden-rgb","source-16px-checker"],
        "raw_cross_boundary_samples":40,"source":"owned-gpui-framebuffer-and-independent-synthetic-oracle",
        "clipboard_untouched":true,"desktop_pixels_read":false,
        "limitations":["fit只验证基线与移动一致性，不证明缩小后的精确原像素", "最大缩放的混合contain相位只以宽色块和裁剪前后一致性验证", "有限抽样不是全部图像逐像素证明"],
    }));
    report.save(None, false)
}
