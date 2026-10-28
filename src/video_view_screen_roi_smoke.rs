//! ignored 自有屏幕小块观测；像素身份独立于 Submitted，不声称硬件 scanout 时间。
use super::*;
use crate::{media_foundation::VideoReader, media_playback_sync::qpc_now_100ns};
use serde_json::{json, Value};
use std::{collections::BTreeSet, mem::size_of};
use windows::Win32::{
    Graphics::Gdi::*,
    System::Com::CoCreateGuid,
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};

const MARKERS: [(u32, u32); 2] = [(24, 64), (232, 144)];
const SIDE: u32 = 64;
const FRAMES: usize = 30;
const PHASE_WAIT: Duration = Duration::from_secs(5);

fn crc(bytes: &[u8]) -> u16 {
    let mut value = 0xffff_u16;
    for byte in bytes {
        value ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            value = (value << 1) ^ if value & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    value
}

fn packet(identity: u32, frame: u16) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&identity.to_le_bytes());
    bytes[4..6].copy_from_slice(&frame.to_le_bytes());
    let check = crc(&bytes[..6]);
    bytes[6..].copy_from_slice(&check.to_le_bytes());
    bytes
}

fn decode(mut sample: impl FnMut(usize) -> u8, identity: u32) -> Option<usize> {
    let mut bytes = [0_u8; 8];
    for bit in 0..64 {
        let value = sample(bit);
        if (90..150).contains(&value) {
            return None;
        }
        bytes[bit / 8] |= u8::from(value >= 150) << (bit % 8);
    }
    (bytes[..4] == identity.to_le_bytes() && bytes[6..] == crc(&bytes[..6]).to_le_bytes())
        .then(|| usize::from(u16::from_le_bytes([bytes[4], bytes[5]])))
        .filter(|frame| *frame < FRAMES)
}

fn fixture(root: &Path) -> Result<(PathBuf, u32, Vec<Duration>)> {
    let _runtime = MfRuntime::new()?;
    let identity = unsafe { CoCreateGuid()? }.data1;
    let path = root.join("screen-roi.mp4");
    ensure(!path.exists(), "屏幕夹具已存在，拒绝覆盖")?;
    let mut writer = Mp4Writer::new(&path, 320, 240, 10, 1_200_000, true)?;
    let mut pixels = vec![24_u8; 320 * 240 * 4];
    for frame in 0..FRAMES {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[24, 24, 24, 255]);
        }
        let bytes = packet(identity, frame as u16);
        for (left, top) in MARKERS {
            for bit in 0..64_u32 {
                let value = if bytes[bit as usize / 8] & (1 << (bit % 8)) != 0 {
                    192
                } else {
                    48
                };
                for y in top + bit / 8 * 8..top + bit / 8 * 8 + 8 {
                    for x in left + bit % 8 * 8..left + bit % 8 * 8 + 8 {
                        pixels[((y * 320 + x) * 4) as usize..][..4]
                            .copy_from_slice(&[value, value, value, 255]);
                    }
                }
            }
        }
        let pts = Duration::from_millis(frame as u64 * 100);
        writer.write_video(&pixels, 320, pts, Duration::from_millis(100))?;
        writer.write_audio(&audio_window::synthetic_audio(frame as u8), pts)?;
    }
    writer.finish()?;
    let mut reader = VideoReader::open(&path)?;
    ensure(
        reader.width == 320 && reader.height == 240,
        "屏幕夹具尺寸变化",
    )?;
    let mut times = Vec::new();
    for _ in 0..FRAMES + 2 {
        let Some(frame) = reader.next()? else {
            ensure(times.len() == FRAMES, "屏幕夹具未完整回读")?;
            return Ok((path, identity, times));
        };
        ensure(frame.bgra.len() == pixels.len(), "夹具回读像素尺寸错误")?;
        for (left, top) in MARKERS {
            let id = decode(
                |bit| {
                    frame.bgra[(((top + bit as u32 / 8 * 8 + 4) * 320
                        + left
                        + bit as u32 % 8 * 8
                        + 4)
                        * 4) as usize]
                },
                identity,
            );
            ensure(id == Some(times.len()), "夹具回读双标记/CRC不匹配")?;
        }
        ensure(
            times.last().is_none_or(|previous| *previous < frame.time),
            "夹具PTS倒退",
        )?;
        times.push(frame.time);
    }
    Err("屏幕夹具回读没有有限结束".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Area {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[link(name = "user32")]
extern "system" {
    #[link_name = "GetWindow"]
    fn window_in_z_order(window: HWND, command: u32) -> HWND;
}
#[link(name = "dwmapi")]
extern "system" {
    fn DwmGetWindowAttribute(window: HWND, attribute: u32, value: *mut u32, bytes: u32) -> i32;
}

/// 中心点命中不能排除细窄遮挡；只查Z序上层窗口元数据，保守拒绝任何相交。
fn unobscured(native: HWND, areas: &[Area; 2]) -> Result<()> {
    let mut above = unsafe { window_in_z_order(native, 3) }; // GW_HWNDPREV
    for _ in 0..512 {
        if above.0.is_null() {
            return Ok(());
        }
        if unsafe { IsWindowVisible(above) }.as_bool() && !unsafe { IsIconic(above) }.as_bool() {
            let mut cloaked = 0;
            ensure(
                unsafe { DwmGetWindowAttribute(above, 14, &mut cloaked, 4) } >= 0,
                "screen_roi无法验：上层窗口状态不明",
            )?; // DWMWA_CLOAKED
            if cloaked == 0 {
                let mut rect = RECT::default();
                unsafe {
                    GetWindowRect(above, &mut rect)?;
                }
                ensure(
                    !areas.iter().any(|area| {
                        rect.left < area.x + area.width
                            && rect.right > area.x
                            && rect.top < area.y + area.height
                            && rect.bottom > area.y
                    }),
                    "screen_roi无法验：上层窗口与标记ROI相交",
                )?;
            }
        }
        above = unsafe { window_in_z_order(above, 3) };
    }
    Err("screen_roi无法验：上层窗口检查超出预算".into())
}

fn ownership(input: Input, geometry: NativeGeometry, dpi: u32, areas: &[Area; 2]) -> Result<()> {
    let native = HWND(input.handle as *mut _);
    ensure(
        native_geometry(input)? == geometry && unsafe { GetDpiForWindow(native) } == dpi,
        "screen_roi无法验：窗口几何/DPI变化",
    )?;
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(native, Some(&mut pid));
    }
    ensure(
        pid == std::process::id()
            && unsafe { IsWindowVisible(native) }.as_bool()
            && unsafe { GetForegroundWindow() } == native,
        "screen_roi无法验：窗口身份/可见/焦点变化",
    )?;
    for area in areas {
        ensure(
            area.x >= geometry.client_origin[0]
                && area.y >= geometry.client_origin[1]
                && area.x + area.width <= geometry.client_origin[0] + geometry.client_size[0]
                && area.y + area.height <= geometry.client_origin[1] + geometry.client_size[1],
            "标记ROI超出自有客户区",
        )?;
        for row in 0..8 {
            for column in 0..8 {
                let point = POINT {
                    x: area.x + area.width * (column * 2 + 1) / 16,
                    y: area.y + area.height * (row * 2 + 1) / 16,
                };
                let owner = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
                ensure(owner == native, "screen_roi无法验：标记被遮挡或归属变化")?;
            }
        }
    }
    unobscured(native, areas)
}

/// 只读两块自有marker；临时DIB不写文件，不回传原始像素。
fn read_marker(area: Area, identity: u32) -> Result<Option<usize>> {
    unsafe {
        let screen = GetDC(None);
        ensure(!screen.is_invalid(), "screen_roi屏幕DC不可用")?;
        let memory = CreateCompatibleDC(Some(screen));
        if memory.is_invalid() {
            ReleaseDC(None, screen);
            return Err("screen_roi内存DC不可用".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: area.width,
                biHeight: -area.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) => bitmap,
            Err(error) => {
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                return Err(error.into());
            }
        };
        let previous = SelectObject(memory, bitmap.into());
        let result = (|| -> Result<_> {
            BitBlt(
                memory,
                0,
                0,
                area.width,
                area.height,
                Some(screen),
                area.x,
                area.y,
                SRCCOPY | CAPTUREBLT,
            )?;
            GdiFlush().ok()?;
            ensure(!bits.is_null(), "screen_roi像素地址无效")?;
            let pixels = std::slice::from_raw_parts(
                bits as *const u8,
                area.width as usize * area.height as usize * 4,
            );
            Ok(decode(
                |bit| {
                    let x = area.width as usize * (bit % 8 * 2 + 1) / 16;
                    let y = area.height as usize * (bit / 8 * 2 + 1) / 16;
                    pixels[(y * area.width as usize + x) * 4]
                },
                identity,
            ))
        })();
        SelectObject(memory, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        result
    }
}

struct Sample {
    frame: usize,
    transport: u64,
    epoch: u64,
    q0: u64,
    q1: u64,
    c0: Duration,
    c1: Duration,
}
struct Probe {
    identity: u32,
    path: PathBuf,
    baseline: Option<(usize, NativeGeometry, u32, [Area; 2])>,
}

impl Probe {
    fn sample(
        &mut self,
        window: WindowHandle<VideoView>,
        playing: bool,
        cx: &mut AsyncApp,
    ) -> Result<Option<Sample>> {
        window.update(cx, |view, native, _| -> Result<_> {
            ensure(
                view.path.as_ref() == Some(&self.path) && view.muted && view.volume == 0.,
                "screen_roi来源或静音状态变化",
            )?;
            ensure(
                view.open_error.is_none() && view.frame_draw_error.is_none(),
                "screen_roi播放器/绘制失败",
            )?;
            let Some(player) = &view.player else {
                return Ok(None);
            };
            if let Some(error) = player.error().or_else(|| player.audio_error()) {
                return Err(error.into());
            }
            let Some(bounds) = view
                .screen_roi_bounds
                .get()
                .filter(|_| view.submitted_frame.is_some())
            else {
                return Ok(None);
            };
            if player.playing() != playing || player.seeking() {
                return Ok(None);
            }
            let scale = native.scale_factor();
            let input = Input {
                handle: crate::gpui_capture::hwnd(native).0 as usize,
                scale,
            };
            let geometry = native_geometry(input)?;
            let dpi = unsafe { GetDpiForWindow(HWND(input.handle as *mut _)) };
            ensure(
                dpi != 0 && (dpi as f32 / 96. - scale).abs() < 0.01,
                "screen_roi无一致的物理DPI",
            )?;
            let areas = MARKERS.map(|(left, top)| {
                let x = geometry.client_origin[0]
                    + ((f32::from(bounds.left())
                        + f32::from(bounds.size.width) * left as f32 / 320.)
                        * scale)
                        .round() as i32;
                let y = geometry.client_origin[1]
                    + ((f32::from(bounds.top())
                        + f32::from(bounds.size.height) * top as f32 / 240.)
                        * scale)
                        .round() as i32;
                Area {
                    x,
                    y,
                    width: (f32::from(bounds.size.width) * SIDE as f32 / 320. * scale).round()
                        as i32,
                    height: (f32::from(bounds.size.height) * SIDE as f32 / 240. * scale).round()
                        as i32,
                }
            });
            ensure(
                areas.iter().all(|area| {
                    (32..=256).contains(&area.width) && (32..=256).contains(&area.height)
                }),
                "screen_roi标记尺寸超出有界读回范围",
            )?;
            let current = (input.handle, geometry, dpi, areas);
            if let Some(baseline) = self.baseline {
                ensure(current == baseline, "screen_roi无法验：宿主/视频布局变化")?;
            }
            ownership(input, geometry, dpi, &areas)?;
            let q0 = qpc_now_100ns().ok_or("screen_roi缺少QPC")?;
            let before = player.audio_clock_at(q0);
            let first = read_marker(areas[0], self.identity)?;
            let second = read_marker(areas[1], self.identity)?;
            let q1 = qpc_now_100ns().ok_or("screen_roi缺少后继QPC")?;
            let after = player.audio_clock_at(q1);
            ownership(input, geometry, dpi, &areas)?;
            self.baseline = Some(current);
            let (Some(frame), Some(before), Some(after)) =
                (first.filter(|id| Some(*id) == second), before, after)
            else {
                return Ok(None);
            };
            let phase = if playing {
                crate::preview_audio_clock::ClockPhase::Playing
            } else {
                crate::preview_audio_clock::ClockPhase::Paused
            };
            if q1 < q0
                || q1 - q0 > 250_000
                || before.clock.qpc_100ns != q0
                || after.clock.qpc_100ns != q1
                || before.transport_generation != after.transport_generation
                || before.clock.generation != after.clock.generation
                || before.clock.phase != phase
                || after.clock.phase != phase
                || before.clock.advancing != playing
                || after.clock.advancing != playing
            {
                return Ok(None);
            }
            ensure(
                after.clock.media_position >= before.clock.media_position,
                "screen_roi同代音频位置倒退",
            )?;
            ensure(
                player.sync_report()["groups"]
                    .as_array()
                    .is_some_and(|groups| {
                        groups.iter().all(|group| {
                            group["stage"] != "presented" || group["accepted_total"] == 0
                        })
                    }),
                "screen_roi不能生成Presented证据",
            )?;
            Ok(Some(Sample {
                frame,
                transport: before.transport_generation,
                epoch: before.clock.generation,
                q0,
                q1,
                c0: before.clock.media_position,
                c1: after.clock.media_position,
            }))
        })?
    }
}

async fn collect(
    probe: &mut Probe,
    window: WindowHandle<VideoView>,
    times: &[Duration],
    playing: bool,
    expected: Option<usize>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let deadline = Instant::now() + PHASE_WAIT;
    let mut points = Vec::new();
    let mut frames = BTreeSet::new();
    let mut previous: Option<Sample> = None;
    let mut started = Instant::now();
    while Instant::now() < deadline {
        if let Some(sample) = probe.sample(window, playing, cx)? {
            if expected.is_some_and(|frame| frame != sample.frame) {
                ensure(previous.is_none(), "screen_roi定位成功后又显示旧帧")?;
                tick(cx).await;
                continue;
            }
            if let Some(old) = previous.as_ref() {
                ensure(
                    (sample.transport, sample.epoch) == (old.transport, old.epoch),
                    "screen_roi采样中传输/设备代次变化，无法验",
                )?;
                ensure(
                    sample.q0 > old.q1 && sample.c0 >= old.c1,
                    "screen_roi采样QPC或音频位置倒退",
                )?;
                ensure(
                    if playing {
                        sample.frame >= old.frame
                    } else {
                        sample.frame == old.frame && sample.c0 == old.c0 && sample.c1 == old.c1
                    },
                    "screen_roi播放倒退或暂停未冻结",
                )?;
            } else {
                started = Instant::now();
            }
            let pts = times[sample.frame].as_secs_f64() * 1000.;
            let low = pts - sample.c1.as_secs_f64() * 1000.;
            let high = pts - sample.c0.as_secs_f64() * 1000.;
            points.push(json!({"frame_id":sample.frame,"pts_ms":pts,"qpc_before_100ns":sample.q0,"qpc_after_100ns":sample.q1,
                "transport":sample.transport,"device_epoch":sample.epoch,"signed_error_ms":[low,high],
                "abs_error_ms":[if low <= 0. && high >= 0. { 0. } else { low.abs().min(high.abs()) },low.abs().max(high.abs())]}));
            frames.insert(sample.frame);
            previous = Some(sample);
            if (playing && frames.len() >= 4)
                || (!playing
                    && points.len() >= 8
                    && started.elapsed() >= Duration::from_millis(200))
            {
                let maximum = points
                    .iter()
                    .map(|point| point["abs_error_ms"][1].as_f64().unwrap())
                    .fold(0., f64::max);
                return Ok(
                    json!({"playing":playing,"samples":points,"distinct_frames":frames.len(),"maximum_abs_error_bound_ms":maximum}),
                );
            }
            ensure(points.len() < 160, "screen_roi采样预算耗尽")?;
        }
        tick(cx).await;
    }
    Err("screen_roi无法验：双标记/CRC、同QPC设备钟或目标画面未在时限内取得".into())
}

fn startup_failure(
    window: WindowHandle<VideoView>,
    stage: &str,
    error: &dyn std::fmt::Display,
    cx: &mut AsyncApp,
) -> String {
    let state = window.update(cx, |view, native, _| {
        let handle = crate::gpui_capture::hwnd(native);
        let player = view.player.as_ref();
        let position = player.map(Player::position);
        let audio = qpc_now_100ns()
            .and_then(|qpc| player.and_then(|player| player.audio_clock_at(qpc)))
            .map(|clock| json!({
                "phase":format!("{:?}",clock.clock.phase),
                "media_ms":clock.clock.media_position.as_secs_f64()*1000.,
                "transport":clock.transport_generation,
                "epoch":clock.clock.generation,
            }));
        json!({
            "stage":stage,"opening":view.opening.is_some(),"player":player.is_some(),
            "open_error":view.open_error,"draw_error":view.frame_draw_error,
            "player_error":player.and_then(Player::error),
            "audio_error":player.and_then(Player::audio_error),
            "playing":player.map(Player::playing),"seeking":player.map(Player::seeking),
            "position_ms":position.map(|value|value.0.as_secs_f64()*1000.),
            "duration_ms":position.map(|value|value.1.as_secs_f64()*1000.),
            "submitted_pts_ms":view.submitted_frame.as_ref().map(|frame|frame.position.as_secs_f64()*1000.),
            "visible":unsafe { IsWindowVisible(handle) }.as_bool(),
            "minimized":unsafe { IsIconic(handle) }.as_bool(),
            "foreground":unsafe { GetForegroundWindow() }==handle,
            "audio_clock":audio,
        })
    });
    let state = state.unwrap_or_else(|error| json!({"window_error":error.to_string()}));
    format!("{error}；startup={state}")
}

async fn exercise(
    path: PathBuf,
    identity: u32,
    times: Vec<Duration>,
    lifecycle: &Lifecycle,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let keys = Rc::new(AdapterStats::default());
    let (window, input) = cx.update(|cx| -> Result<_> {
        let window = VideoView::open_paths(vec![path.clone()], cx)?;
        lifecycle.window.set(Some(window));
        let input = window.update(cx, |view, native, _| -> Result<_> {
            ensure(view.player.is_none(), "必须先于播放器设置静音")?;
            view.muted = true;
            view.volume = 0.;
            view.recent_recorded = true;
            let input = Input {
                handle: crate::gpui_capture::hwnd(native).0 as usize,
                scale: native.scale_factor(),
            };
            text_contract_smoke::install(input, &keys)?;
            Ok(input)
        })??;
        Ok((window, input))
    })??;
    // 首次窗口绘制可能晚于短片结束；先取得真实paint/ACK预热，再从确定的起点验证。
    // 预热不计入ROI成功，也不使用Submitted序号推导屏幕标记身份。
    let warmup = wait_frame(window, cx, "屏幕夹具首帧没有提交", |frame| {
        frame.path == path
    })
    .await
    .map_err(|error| startup_failure(window, "warmup", &error, cx))?;
    window.update(cx, |view, _, _| -> Result<()> {
        let player = view.player.as_mut().ok_or("屏幕夹具缺少播放器")?;
        player.set_playing(false);
        player.seek_ratio(0.);
        Ok(())
    })??;
    wait_frame(
        window,
        cx,
        "屏幕夹具归零没有提交新画面",
        |frame| {
            !frame.playing
                && !frame.seeking
                && frame.position.is_zero()
                && frame.pts == times[0]
                && frame.serial > warmup.serial
        },
    )
    .await
    .map_err(|error| startup_failure(window, "paused_origin", &error, cx))?;
    window.update(cx, |view, _, _| -> Result<()> {
        view.player
            .as_mut()
            .ok_or("屏幕夹具缺少播放器")?
            .set_playing(true);
        Ok(())
    })??;
    wait_frame(
        window,
        cx,
        "屏幕夹具归零后没有实际播放推进",
        |frame| frame.playing && frame.pts >= times[1],
    )
    .await
    .map_err(|error| startup_failure(window, "playing_from_origin", &error, cx))?;
    let mut probe = Probe {
        identity,
        path,
        baseline: None,
    };
    let play = collect(&mut probe, window, &times, true, None, cx).await?;
    text_contract_smoke::key(input, VK_SPACE, false, false, false)?;
    paused_frame(window, cx).await?;
    let pause = collect(&mut probe, window, &times, false, None, cx).await?;
    let (target, point, seek_input) = slider_point(window, input, 0.65, cx)?;
    let expected = times
        .iter()
        .rposition(|time| *time <= target)
        .ok_or("屏幕seek目标没有真实PTS")?;
    seek_input.click(point);
    let seek = collect(&mut probe, window, &times, false, Some(expected), cx).await?;
    ensure(
        keys.keys.get() == 1 && keys.restored.get() == 1 && keys.errors.get() == 0,
        "屏幕探针按键未完整恢复",
    )?;
    Ok(
        json!({"ok":true,"screen_roi":{"backend":"screen_dc_bitblt","verified":true,"play":play,"pause":pause,"seek":seek,
        "startup_warmup_pts_ms":warmup.pts.as_secs_f64()*1000.,"startup_reanchored_zero":true,
        "seek_target_ms":target.as_secs_f64()*1000.,"fixture_frame_interval_ms":100,"sync_error_budget_enforced":false,
        "qpc_scope":"readback_interval_not_first_visibility","raw_pixels_saved":false,"whole_screen_read":false},
        "presented_verified":false,"physical_scanout_verified":false,"audible_playback_verified":false}),
    )
}

pub(super) fn run() -> Result<()> {
    let fixtures = Fixtures::new()?;
    let (path, identity, times) = fixture(&fixtures.root)?;
    let result = Rc::new(RefCell::new(Some("screen_roi未完成".to_string())));
    let finished = result.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move |cx| {
        if let Err(error) = cx.open_window(WindowOptions { show: false, focus: false, ..Default::default() }, |_, cx| cx.new(|_| Host)) {
            *result.borrow_mut() = Some(error.to_string()); cx.quit(); return;
        }
        cx.spawn(async move |cx| {
            let lifecycle = Lifecycle::default();
            let outcome = exercise(path, identity, times, &lifecycle, cx).await;
            let cleanup = close_owned(&lifecycle, cx).await;
            let failure = match (outcome, cleanup) {
                (Ok(report), Ok(())) => { println!("{report}"); None },
                (Err(error), Ok(())) | (Ok(_), Err(error)) => Some(error.to_string()),
                (Err(error), Err(cleanup)) => Some(format!("{error}；screen_roi退出失败：{cleanup}")),
            };
            if let Some(error) = &failure {
                println!("{}", json!({"ok":false,"screen_roi":{"verified":false,"error":error},"presented_verified":false}));
            }
            *result.borrow_mut() = failure;
            let _ = cx.update(|cx| { for window in cx.windows() { let _ = cx.update_window(window, |_, window, _| window.remove_window()); } cx.quit(); });
        }).detach();
    });
    let error = finished.borrow_mut().take();
    error.map_or(Ok(()), |error| Err(error.into()))
}
