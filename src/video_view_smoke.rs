//! 显式自有窗口验收：原有场景不读桌面或剪贴板；独立ROI专项仅读自有视频标记。
use super::*;
use crate::{
    gpui_smoke::Input,
    media_foundation::{MfRuntime, Mp4Writer},
    recording_preview::PlaybackShutdownProbe,
    text_contract_smoke::{self, AdapterStats},
};
use gpui::{AppContext, AsyncApp};
use std::{ffi::OsString, path::Path, sync::Weak};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        ClientToScreen, GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO,
        MONITOR_DEFAULTTONEAREST,
    },
    UI::{
        Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_F11, VK_SPACE},
        WindowsAndMessaging::{
            GetClientRect, GetWindowRect, IsIconic, IsWindow, PostMessageW, ShowWindow,
            SW_MINIMIZE, SW_RESTORE, WM_CLOSE,
        },
    },
};

const WAIT: Duration = Duration::from_secs(20);

#[path = "video_view_audio_window_smoke.rs"]
mod audio_window;
#[path = "video_view_embedded_smoke.rs"]
mod embedded_preview;
#[path = "video_view_screen_roi_smoke.rs"]
mod screen_roi;

#[derive(Clone, Copy)]
enum Scenario {
    Silent,
    Fullscreen,
    EmbeddedAudio,
}

impl Scenario {
    fn with_audio(self) -> bool {
        matches!(self, Self::EmbeddedAudio)
    }

    fn check_fullscreen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }
}

#[derive(Default)]
struct Lifecycle {
    window: Cell<Option<WindowHandle<VideoView>>>,
    workers: RefCell<Vec<PlaybackShutdownProbe>>,
}

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

struct Fixtures {
    root: PathBuf,
    previous_data: Option<OsString>,
}

impl Fixtures {
    fn new() -> Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustshot-video-window-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root)?;
        let previous_data = std::env::var_os("RUSTSHOT_DATA_DIR");
        // 空的自有配置使用默认System主题；不修改维护者的偏好文件。
        std::env::set_var("RUSTSHOT_DATA_DIR", &root);
        Ok(Self {
            root,
            previous_data,
        })
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        match &self.previous_data {
            Some(value) => std::env::set_var("RUSTSHOT_DATA_DIR", value),
            None => std::env::remove_var("RUSTSHOT_DATA_DIR"),
        }
        // 只清理本次独占目录的直接文件，不递归进入其它目录。
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.root);
    }
}

fn write_fixtures(root: &Path, scenario: Scenario) -> Result<Vec<PathBuf>> {
    // 生成后先释放此线程的MF/COM，再初始化GPUI应用。
    let _runtime = MfRuntime::new()?;
    let mut paths = Vec::new();
    for source in 0..2 {
        let path = root.join(format!("片段 {source}.mp4"));
        let (width, height) = (320_u32, 240_u32);
        let mut writer = Mp4Writer::new(&path, width, height, 10, 600_000, scenario.with_audio())?;
        let mut pixels = vec![0; (width * height * 4) as usize];
        for frame in 0..30_u8 {
            for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
                let left = index % (width as usize) < width as usize / 2;
                let color = match (source, left) {
                    (0, true) => [16, 32 + frame * 3, 210, 255],
                    (_, true) => [16, 210, 32 + frame * 3, 255],
                    (_, false) => [210, 32, 16, 255],
                };
                pixel.copy_from_slice(&color);
            }
            writer.write_video(
                &pixels,
                width,
                Duration::from_millis(u64::from(frame) * 100),
                Duration::from_millis(100),
            )?;
            if scenario.with_audio() {
                writer.write_audio(
                    &audio_window::synthetic_audio(frame),
                    Duration::from_millis(u64::from(frame) * 100),
                )?;
            }
        }
        writer.finish()?;
        paths.push(path);
    }
    Ok(paths)
}

struct Host;

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct Painted {
    generation: u64,
    serial: u64,
    pts: Duration,
    position: Duration,
    duration: Duration,
    playing: bool,
    seeking: bool,
    path: PathBuf,
}

fn painted(window: WindowHandle<VideoView>, cx: &mut AsyncApp) -> Result<Option<Painted>> {
    window.update(cx, |view, _, _| -> Result<_> {
        if let Some(error) = &view.open_error {
            return Err(format!("视频预检失败：{error}").into());
        }
        if let Some(error) = &view.frame_draw_error {
            return Err(error.clone().into());
        }
        let Some(player) = &view.player else {
            return Ok(None);
        };
        if let Some(error) = player.error() {
            return Err(error.into());
        }
        if let Some(frame) = &view.submitted_frame {
            ensure(
                player.submitted_frame_point() == Some((frame.serial, frame.position)),
                "窗口场景提交未收到播放器ACK",
            )?;
        }
        // 只读生产paint成功分支写入的回执，不抢走Player的单槽帧。
        Ok(view.submitted_frame.as_ref().map(|frame| Painted {
            generation: view.source_generation,
            serial: frame.serial,
            pts: frame.position,
            position: player.position().0,
            duration: player.position().1,
            playing: player.playing(),
            seeking: player.seeking(),
            path: view.path.clone().unwrap_or_default(),
        }))
    })?
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}

async fn wait_frame(
    window: WindowHandle<VideoView>,
    cx: &mut AsyncApp,
    message: &str,
    ready: impl Fn(&Painted) -> bool,
) -> Result<Painted> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(frame) = painted(window, cx)? {
            if ready(&frame) {
                return Ok(frame);
            }
        }
        ensure(Instant::now() < deadline, message)?;
        tick(cx).await;
    }
}

/// 只在首个等待失败时采样；错误正文可能含来源路径，因此只记录各错误是否存在。
fn log_initial_frame_failure(window: WindowHandle<VideoView>, cx: &mut AsyncApp) {
    use std::io::Write;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible};

    const LABEL: &str = "VIDEO_VIEW_INITIAL_FRAME_DIAGNOSTIC ";
    const MAX_LOG_BYTES: usize = 16 * 1024;
    let sampled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        window.update(cx, |view, native, _| {
            let player = view.player.as_ref();
            let position = player.map(Player::position);
            let ack = player.and_then(Player::submitted_frame_point);
            let qpc = crate::media_playback_sync::qpc_now_100ns();
            let audio_clock = qpc
                .and_then(|qpc| player.and_then(|player| player.audio_clock_at(qpc)))
                .map(|published| serde_json::json!({
                    "transport_generation":published.transport_generation,
                    "epoch":published.clock.generation,
                    "phase":format!("{:?}",published.clock.phase),
                    "advancing":published.clock.advancing,
                    "position_ns":published.clock.media_position.as_nanos().to_string(),
                    "queued_ns":published.clock.queued_media.as_nanos().to_string(),
                    "qpc_100ns":published.clock.qpc_100ns,
                }));
            let handle = crate::gpui_capture::hwnd(native);
            let viewport = native.viewport_size();
            serde_json::json!({
                "stage":"initial_playing_frame_wait_failed",
                "opening":view.opening.is_some(),"player":player.is_some(),
                "presentation":if view.presentation == Presentation::Window { "window" } else { "embedded" },
                "source_generation":view.source_generation,
                "image":view.image.as_ref().map(|frame| serde_json::json!({
                    "serial":frame.serial,"pts_ns":frame.position.as_nanos().to_string(),
                })),
                "submitted":view.submitted_frame.as_ref().map(|frame| serde_json::json!({
                    "serial":frame.serial,"pts_ns":frame.position.as_nanos().to_string(),
                    "ack_matches":ack == Some((frame.serial,frame.position)),
                })),
                "player_ack":ack.map(|(serial,pts)| serde_json::json!({
                    "serial":serial,"pts_ns":pts.as_nanos().to_string(),
                })),
                "playing":player.map(Player::playing),"seeking":player.map(Player::seeking),
                "position_ns":position.map(|value|value.0.as_nanos().to_string()),
                "duration_ns":position.map(|value|value.1.as_nanos().to_string()),
                "error_presence":{
                    "open":view.open_error.is_some(),"draw":view.frame_draw_error.is_some(),
                    "player":player.is_some_and(|player|player.error().is_some()),
                    "audio":player.is_some_and(|player|player.audio_error().is_some()),
                },
                "error_text_omitted_to_exclude_source_paths":true,
                "sync_report":player.map(Player::sync_report),
                "native":{
                    "valid":unsafe { IsWindow(Some(handle)) }.as_bool(),
                    "visible":unsafe { IsWindowVisible(handle) }.as_bool(),
                    "iconic":unsafe { IsIconic(handle) }.as_bool(),
                    "foreground_is_self":!handle.is_invalid() && unsafe { GetForegroundWindow() } == handle,
                    "viewport_width":f32::from(viewport.width),
                    "viewport_height":f32::from(viewport.height),"scale":native.scale_factor(),
                },
                "qpc_100ns":qpc,"audio_clock":audio_clock,
                "audio_info":player.and_then(Player::audio_info),
                "audio_switching":player.map(Player::audio_switching),
            })
        })
    }));
    let report = match sampled {
        Ok(Ok(report)) => report,
        Ok(Err(_)) => serde_json::json!({"snapshot_failed":"window_unavailable"}),
        Err(_) => serde_json::json!({"snapshot_failed":"panic_contained"}),
    };
    let encoded = match serde_json::to_string(&report) {
        Ok(encoded) if encoded.len() + LABEL.len() + 1 <= MAX_LOG_BYTES => encoded,
        Ok(encoded) => serde_json::json!({
            "truncated":true,"log_limit_bytes":MAX_LOG_BYTES,
            "complete_json_bytes":encoded.len(),"complete_report_logged":false,
        })
        .to_string(),
        Err(_) => "{\"snapshot_failed\":\"serialization_failed\"}".into(),
    };
    // 诊断写出失败也不替换原等待错误。
    let _ = writeln!(std::io::stderr().lock(), "{LABEL}{encoded}");
}

async fn paused_frame(window: WindowHandle<VideoView>, cx: &mut AsyncApp) -> Result<Painted> {
    let deadline = Instant::now() + WAIT;
    let mut previous = None;
    let mut stable_since = Instant::now();
    let mut diagnostic = serde_json::Value::Null;
    loop {
        if let Some(frame) = painted(window, cx)? {
            let pause_clock = audio_window::pause_clock_observation(window, cx)?;
            let identity = (frame.generation, frame.serial, frame.pts, frame.position);
            diagnostic = serde_json::json!({
                "source_generation":frame.generation,"serial":frame.serial,
                "pts_ns":frame.pts.as_nanos().to_string(),
                "position_ns":frame.position.as_nanos().to_string(),
                "playing":frame.playing,"seeking":frame.seeking,
                "audio":pause_clock.diagnostic,
            });
            if !frame.playing
                && !frame.seeking
                && frame.pts + Duration::from_millis(100) < frame.duration
                && frame.position < frame.duration
                && pause_clock.ready
                && previous == Some(identity)
            {
                if stable_since.elapsed() >= Duration::from_millis(200) {
                    return Ok(frame);
                }
            } else {
                // 未确认设备暂停时不能积累稳定时间；初次就绪后仍完整观察200ms。
                previous = pause_clock.ready.then_some(identity);
                stable_since = Instant::now();
            }
        }
        ensure(
            Instant::now() < deadline,
            &format!("暂停后场景帧未稳定：{diagnostic}"),
        )?;
        tick(cx).await;
    }
}

struct PlaybackObservation {
    generation: u64,
    path: PathBuf,
    position: Duration,
    duration: Duration,
    playing: bool,
    seeking: bool,
}

fn playback_observation(
    window: WindowHandle<VideoView>,
    cx: &mut AsyncApp,
) -> Result<PlaybackObservation> {
    window.update(cx, |view, _, _| -> Result<_> {
        let player = view.player.as_ref().ok_or("窗口没有可观察的播放器")?;
        if let Some(error) = player.error() {
            return Err(error.into());
        }
        let (position, duration) = player.position();
        Ok(PlaybackObservation {
            generation: view.source_generation,
            path: view.path.clone().ok_or("播放器缺少来源路径")?,
            position,
            duration,
            playing: player.playing(),
            seeking: player.seeking(),
        })
    })?
}

async fn minimize_and_wait_paused(
    window: WindowHandle<VideoView>,
    input: Input,
    expected: &Painted,
    cx: &mut AsyncApp,
) -> Result<PlaybackObservation> {
    let native = HWND(input.handle as *mut _);
    // ShowWindow返回之前是否可见，不用其返回值判定最小化是否成功。
    let _ = unsafe { ShowWindow(native, SW_MINIMIZE) };
    let deadline = Instant::now() + WAIT;
    let mut stable = None;
    loop {
        let observed = playback_observation(window, cx)?;
        ensure(
            observed.generation == expected.generation && observed.path == expected.path,
            "最小化期间视频来源发生变化",
        )?;
        if !expected.playing {
            if observed.playing || observed.seeking || observed.position != expected.position {
                let clock = audio_window::pause_clock_observation(window, cx)
                    .map(|observation| observation.diagnostic)
                    .unwrap_or_else(|error| serde_json::json!({"error":error.to_string()}));
                return Err(format!(
                    "原暂停视频在最小化期间意外播放或位置前进：{}",
                    serde_json::json!({
                        "expected":{
                            "source_generation":expected.generation,"serial":expected.serial,
                            "pts_ns":expected.pts.as_nanos().to_string(),
                            "position_ns":expected.position.as_nanos().to_string(),
                            "playing":expected.playing,"seeking":expected.seeking,
                        },
                        "observed":{
                            "source_generation":observed.generation,
                            "position_ns":observed.position.as_nanos().to_string(),
                            "playing":observed.playing,"seeking":observed.seeking,
                            "iconic":unsafe { IsIconic(native) }.as_bool(),
                        },
                        "audio":clock,
                    }),
                )
                .into());
            }
        }
        if unsafe { IsIconic(native) }.as_bool() && !observed.playing && !observed.seeking {
            // 不能以自然播放到EOF冒充最小化暂停；位置需在片尾之前稳定200ms。
            ensure(
                observed.duration.saturating_sub(observed.position) > Duration::from_millis(100),
                "最小化检查前视频已到片尾，不能证明自动暂停",
            )?;
            match stable {
                Some((position, since)) if position == observed.position => {
                    if Instant::now().duration_since(since) >= Duration::from_millis(200) {
                        return Ok(observed);
                    }
                }
                _ => stable = Some((observed.position, Instant::now())),
            }
        } else {
            stable = None;
        }
        ensure(
            Instant::now() < deadline,
            "最小化后播放状态或实际位置未暂停稳定",
        )?;
        tick(cx).await;
    }
}

async fn restore_and_keep_paused(
    window: WindowHandle<VideoView>,
    input: Input,
    expected: &PlaybackObservation,
    cx: &mut AsyncApp,
) -> Result<()> {
    let native = HWND(input.handle as *mut _);
    let _ = unsafe { ShowWindow(native, SW_RESTORE) };
    let deadline = Instant::now() + WAIT;
    while unsafe { IsIconic(native) }.as_bool() {
        ensure(Instant::now() < deadline, "原暂停视频的窗口未还原")?;
        tick(cx).await;
    }
    let stable_since = Instant::now();
    loop {
        let observed = playback_observation(window, cx)?;
        ensure(
            observed.generation == expected.generation && observed.path == expected.path,
            "还原暂停窗口时视频来源发生变化",
        )?;
        ensure(
            !observed.playing && !observed.seeking && observed.position == expected.position,
            "原暂停视频在最小化还原后意外播放或位置前进",
        )?;
        ensure(
            !unsafe { IsIconic(native) }.as_bool(),
            "暂停窗口还原后再次最小化",
        )?;
        if stable_since.elapsed() >= Duration::from_millis(200) {
            return Ok(());
        }
        ensure(Instant::now() < deadline, "还原后未完成暂停稳定检查")?;
        tick(cx).await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
struct NativeGeometry {
    outer: [i32; 4],
    client_origin: [i32; 2],
    client_size: [i32; 2],
}

fn native_geometry(input: Input) -> Result<NativeGeometry> {
    let native = HWND(input.handle as *mut _);
    ensure(
        unsafe { IsWindow(Some(native)) }.as_bool() && !unsafe { IsIconic(native) }.as_bool(),
        "全屏检查窗口已关闭或最小化",
    )?;
    let (mut outer, mut client, mut origin) = (RECT::default(), RECT::default(), POINT::default());
    unsafe {
        GetWindowRect(native, &mut outer)?;
        GetClientRect(native, &mut client)?;
        ClientToScreen(native, &mut origin).ok()?;
    }
    Ok(NativeGeometry {
        outer: [outer.left, outer.top, outer.right, outer.bottom],
        client_origin: [origin.x, origin.y],
        client_size: [client.right - client.left, client.bottom - client.top],
    })
}

async fn wait_fullscreen_geometry(
    window: WindowHandle<VideoView>,
    input: Input,
    paused: &Painted,
    expected: NativeGeometry,
    monitor: HMONITOR,
    fullscreen: bool,
    cx: &mut AsyncApp,
) -> Result<NativeGeometry> {
    let deadline = Instant::now() + WAIT;
    let mut stable_since = None;
    loop {
        let frame = painted(window, cx)?.ok_or("全屏切换期间丢失已提交画面")?;
        ensure(
            frame.generation == paused.generation && frame.path == paused.path,
            "全屏切换期间视频来源发生变化",
        )?;
        ensure(
            !frame.playing
                && !frame.seeking
                && frame.position == paused.position
                && frame.pts == paused.pts,
            "全屏切换期间暂停状态或实际位置发生变化",
        )?;
        let reported = window.update(cx, |_, native, _| -> Result<_> {
            ensure(
                crate::gpui_capture::hwnd(native).0 as usize == input.handle,
                "全屏检查窗口身份发生变化",
            )?;
            Ok(native.is_fullscreen())
        })??;
        let geometry = native_geometry(input)?;
        let current_monitor =
            unsafe { MonitorFromWindow(HWND(input.handle as *mut _), MONITOR_DEFAULTTONEAREST) };
        // 原生物理边界是主证据；框架状态仅作附加核对，并等待异步切换稳定。
        if geometry == expected && current_monitor == monitor && reported == fullscreen {
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(200) {
                return Ok(geometry);
            }
        } else {
            stable_since = None;
        }
        ensure(
            Instant::now() < deadline,
            if fullscreen {
                "F11未将窗口及客户区铺满原显示器"
            } else {
                "F11未将窗口位置、尺寸及客户区还原"
            },
        )?;
        tick(cx).await;
    }
}

async fn fullscreen_restore(
    window: WindowHandle<VideoView>,
    input: Input,
    paused: &Painted,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let before = native_geometry(input)?;
    let native = HWND(input.handle as *mut _);
    let monitor = unsafe { MonitorFromWindow(native, MONITOR_DEFAULTTONEAREST) };
    ensure(!monitor.is_invalid(), "全屏检查没有当前显示器")?;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    ensure(
        unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
        "全屏检查无法取得显示器物理边界",
    )?;
    let bounds = info.rcMonitor;
    let full = NativeGeometry {
        outer: [bounds.left, bounds.top, bounds.right, bounds.bottom],
        client_origin: [bounds.left, bounds.top],
        client_size: [bounds.right - bounds.left, bounds.bottom - bounds.top],
    };
    ensure(before.outer != full.outer, "全屏检查初始窗口已铺满显示器")?;
    wait_fullscreen_geometry(window, input, paused, before, monitor, false, cx).await?;
    text_contract_smoke::key(input, VK_F11, false, false, false)?;
    let entered = wait_fullscreen_geometry(window, input, paused, full, monitor, true, cx).await?;
    text_contract_smoke::key(input, VK_F11, false, false, false)?;
    let restored =
        wait_fullscreen_geometry(window, input, paused, before, monitor, false, cx).await?;
    Ok(serde_json::json!({
        "monitor":monitor.0 as usize,"monitor_bounds":full.outer,
        "before":before,"fullscreen":entered,"restored":restored,
    }))
}

fn image_refs(view: &VideoView) -> Vec<Weak<RenderImage>> {
    view.image
        .iter()
        .chain(view.submitted_frame.iter())
        .map(|frame| Arc::downgrade(&frame.image))
        .chain(view.retiring.iter().map(Arc::downgrade))
        .chain(view.retired.iter().map(Arc::downgrade))
        .collect()
}

fn verify_snapshot(window: WindowHandle<VideoView>, cx: &mut AsyncApp, green: bool) -> Result<()> {
    window.update(cx, |view, _, _| -> Result<()> {
        let frame = view.submitted_frame.as_ref().ok_or("缺少场景提交帧")?;
        let snapshot = view.frame_snapshot()?;
        ensure(snapshot.position == frame.position, "输出时间不是场景帧PTS")?;
        let image = snapshot.to_image();
        ensure((image.width, image.height) == (320, 240), "输出尺寸错误")?;
        ensure(
            Some(image.bgra.as_slice()) == frame.image.as_bytes(0),
            "输出像素不是实际提交帧",
        )?;
        let at = ((image.height / 2 * image.width + image.width / 4) * 4) as usize;
        let pixel = &image.bgra[at..at + 4];
        ensure(
            if green {
                pixel[1] > 170 && pixel[2] < 150
            } else {
                pixel[2] > 170 && pixel[1] < 150
            },
            "场景帧不是当前来源的预期颜色",
        )
    })?
}

fn slider_point(
    window: WindowHandle<VideoView>,
    input: Input,
    fraction: f32,
    cx: &mut AsyncApp,
) -> Result<(Duration, (f32, f32), Input)> {
    window.update(cx, |view, native, _| -> Result<_> {
        let bounds = view.seek_bounds.get().ok_or("进度条尚未完成布局")?;
        let scale = native.scale_factor();
        let point = (
            f32::from(bounds.left() + bounds.size.width * fraction),
            f32::from(bounds.top() + bounds.size.height * 0.5),
        );
        // 原生鼠标按物理像素取整，目标必须由同一量化坐标反算。
        let logical_x = (point.0 * scale).round() / scale;
        let ratio = ratio_at(&view.seek_bounds, px(logical_x)).ok_or("进度条宽度无效")?;
        let duration = view.player.as_ref().ok_or("进度条缺少播放器")?.position().1;
        Ok((
            duration.mul_f32(ratio),
            point,
            Input {
                handle: input.handle,
                scale,
            },
        ))
    })?
}

async fn exercise(
    paths: Vec<PathBuf>,
    lifecycle: &Lifecycle,
    scenario: Scenario,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let keys = Rc::new(AdapterStats::default());
    let (window, input, entity) = cx.update(|cx| -> Result<_> {
        let window = VideoView::open_paths(paths.clone(), cx)?;
        lifecycle.window.set(Some(window));
        let (input, entity) = window.update(cx, |view, native, cx| -> Result<_> {
            ensure(view.player.is_none(), "必须在播放器构造前设置静音及零音量")?;
            view.volume = 0.;
            view.muted = true;
            // MRU走品牌目录而非Preferences覆盖路径；本专项不向用户MRU写夹具。
            // 首帧尚在异步预检中，同线程设置可在paint之前生效。
            view.recent_recorded = true;
            let handle = crate::gpui_capture::hwnd(native);
            let input = Input {
                handle: handle.0 as usize,
                scale: native.scale_factor(),
            };
            // 复用自有消息适配器，每次保存/恢复UI线程键盘状态，不受用户修饰键影响。
            text_contract_smoke::install(input, &keys)?;
            Ok((input, cx.entity().downgrade()))
        })??;
        Ok((window, input, entity))
    })??;
    let first = wait_frame(window, cx, "真实视频窗口未提交播放帧", |frame| {
        frame.playing && frame.pts >= Duration::from_millis(100)
    })
    .await
    .map_err(|error| {
        log_initial_frame_failure(window, cx);
        error
    })?;
    ensure(first.path == paths[0], "首个场景来源错误")?;
    let mut audio_phases = Vec::new();
    if scenario.with_audio() {
        let before = audio_window::capture(window, cx)?;
        audio_phases.push(
            audio_window::collect_playing(window, before, "initial", "production_open", 1., cx)
                .await?,
        );
    }
    let minimized_playing = minimize_and_wait_paused(window, input, &first, cx).await?;
    let before_restore = window.update(cx, |view, _, _| {
        view.submitted_frame.as_ref().map(|frame| frame.serial)
    })?;
    let restore_audio_before = if scenario.with_audio() {
        Some(audio_window::capture(window, cx)?)
    } else {
        None
    };
    let _ = unsafe { ShowWindow(HWND(input.handle as *mut _), SW_RESTORE) };
    let restored_playing = wait_frame(
        window,
        cx,
        "还原后未恢复同源播放并提交新画面",
        |frame| {
            !unsafe { IsIconic(HWND(input.handle as *mut _)) }.as_bool()
                && frame.generation == first.generation
                && frame.path == first.path
                && frame.playing
                && !frame.seeking
                && Some(frame.serial) != before_restore
                && frame.position > minimized_playing.position
        },
    )
    .await?;
    if let Some(before) = restore_audio_before {
        audio_phases.push(
            audio_window::collect_playing(window, before, "restore", "native_restore", 1., cx)
                .await?,
        );
    }
    text_contract_smoke::key(input, VK_SPACE, false, false, false)?;
    let paused = paused_frame(window, cx).await?;
    let minimized_paused = minimize_and_wait_paused(window, input, &paused, cx).await?;
    ensure(
        minimized_paused.position == paused.position,
        "原暂停视频在最小化期间实际位置前进",
    )?;
    restore_and_keep_paused(window, input, &minimized_paused, cx).await?;
    let paused = paused_frame(window, cx).await?;
    let fullscreen_geometry = if scenario.check_fullscreen() {
        Some(fullscreen_restore(window, input, &paused, cx).await?)
    } else {
        None
    };
    verify_snapshot(window, cx, false)?;
    ensure(
        paused.duration >= Duration::from_millis(2900)
            && paused.duration <= Duration::from_millis(3100),
        "合成视频时长错误",
    )?;
    let (target, point, seek_input) = slider_point(window, input, 0.5, cx)?;
    let expected_pts = Duration::from_millis((target.as_millis() / 100 * 100) as u64);
    seek_input.click(point);
    wait_frame(window, cx, "暂停定位后未提交目标帧", |frame| {
        frame.serial != paused.serial
            && frame.pts == expected_pts
            && !frame.playing
            && !frame.seeking
    })
    .await?;
    let seeked = paused_frame(window, cx).await?;
    text_contract_smoke::key(input, VIRTUAL_KEY(u16::from(b'N')), false, false, false)?;
    let stepped = wait_frame(window, cx, "逐帧未按场景提交帧推进", |frame| {
        frame.serial != seeked.serial
            && frame.pts == seeked.pts + Duration::from_millis(100)
            && !frame.playing
            && !frame.seeking
    })
    .await?;
    let audio_keys = if scenario.with_audio() {
        let (phases, keys) = audio_window::exercise_controls(window, input, &stepped, cx).await?;
        audio_phases.extend(phases);
        keys
    } else {
        0
    };
    let (old_images, old_workers) = window.update(cx, |view, _, cx| -> Result<_> {
        let old_images = image_refs(view);
        let old_workers = view.player.as_ref().ok_or("缺少旧播放器")?.shutdown_probe();
        lifecycle.workers.borrow_mut().push(old_workers.clone());
        view.navigate_files(1, cx);
        view.recent_recorded = true;
        ensure(
            view.player.is_none() && view.muted && view.volume == 0.,
            "切源后必须在新播放器构造前保留静音及零音量",
        )?;
        ensure(
            view.image.is_none() && view.submitted_frame.is_none(),
            "切源后旧场景帧未立即失效",
        )?;
        Ok((old_images, old_workers))
    })??;
    let switched = wait_frame(window, cx, "切源后未提交新文件帧", |frame| {
        frame.generation != stepped.generation && frame.path == paths[1]
    })
    .await?;
    verify_snapshot(window, cx, true)?;
    if scenario.with_audio() {
        let before = audio_window::capture(window, cx)?;
        audio_phases.push(
            audio_window::collect_playing(
                window,
                before,
                "source_switch",
                "production_queue_navigation",
                1.,
                cx,
            )
            .await?,
        );
    }
    let deadline = Instant::now() + WAIT;
    while old_images.iter().any(|image| image.upgrade().is_some()) || !old_workers.finished() {
        ensure(
            Instant::now() < deadline,
            "切源后旧场景图像或媒体后台未回收",
        )?;
        tick(cx).await;
    }
    let (final_images, final_workers) = window.update(cx, |view, _, _| -> Result<_> {
        Ok((
            image_refs(view),
            view.player
                .as_ref()
                .ok_or("缺少待关闭播放器")?
                .shutdown_probe(),
        ))
    })??;
    lifecycle.workers.borrow_mut().push(final_workers.clone());
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        )?;
    }
    let deadline = Instant::now() + WAIT;
    while entity.upgrade().is_some()
        || unsafe { IsWindow(Some(HWND(input.handle as *mut _))) }.as_bool()
        || final_images.iter().any(|image| image.upgrade().is_some())
        || !final_workers.finished()
        || super::has_pending_work()
    {
        ensure(
            Instant::now() < deadline,
            "关闭后窗口、实体、场景图像或媒体后台未回收",
        )?;
        tick(cx).await;
    }
    let expected_keys = 2 + if scenario.check_fullscreen() { 2 } else { 0 } + audio_keys;
    ensure(
        keys.keys.get() == expected_keys
            && keys.restored.get() == expected_keys
            && keys.errors.get() == 0,
        "原生按键未完整处理或UI线程键盘状态未恢复",
    )?;
    Ok(serde_json::json!({
        "ok":true,"suite":"video-view-window","silent_fixture":!scenario.with_audio(),
        "paint_submitted":true,"player_ack_matches":true,
        "native_pause_seek_step":true,"source_switch":true,
        "fullscreen_checked":scenario.check_fullscreen(),
        "fullscreen_restore":fullscreen_geometry.as_ref().map(|_| true),
        "fullscreen_geometry":fullscreen_geometry,
        "embedded_audio_checked":scenario.with_audio(),"audio_phases":audio_phases,
        "output_muted":true,"audible_playback_verified":false,
        "native_key_count":expected_keys,
        "minimize_pause_restore":true,"paused_minimize_stays_paused":true,
        "minimized_position_stable_ms":200,
        "minimized_position_ms":minimized_playing.position.as_millis(),
        "restored_submitted_pts_ms":restored_playing.pts.as_millis(),
        "first_pts_ms":first.pts.as_millis(),"seek_target_ms":target.as_millis(),
        "seek_pts_ms":seeked.pts.as_millis(),
        "step_pts_ms":stepped.pts.as_millis(),"next_source_pts_ms":switched.pts.as_millis(),
        "hwnd_entity_images_released":true,"worker_exit_observed":true,
        "desktop_pixels_read":false,"clipboard_untouched":true,"presented_verified":false
    }))
}

async fn close_owned(lifecycle: &Lifecycle, cx: &mut AsyncApp) -> Result<()> {
    let native = lifecycle.window.get().and_then(|window| {
        window
            .update(cx, |view, window, _| {
                if let Some(player) = &view.player {
                    lifecycle.workers.borrow_mut().push(player.shutdown_probe());
                }
                let native = crate::gpui_capture::hwnd(window).0 as usize;
                window.remove_window();
                native
            })
            .ok()
    });
    // 失败也先关闭自有窗口，再等媒体资源退出，最后才退出App和删除夹具。
    let deadline = Instant::now() + WAIT;
    while lifecycle
        .workers
        .borrow()
        .iter()
        .any(|probe| !probe.finished())
        || native.is_some_and(|native| unsafe { IsWindow(Some(HWND(native as *mut _))) }.as_bool())
        || super::has_pending_work()
    {
        ensure(
            Instant::now() < deadline,
            "失败收尾时媒体后台或自有窗口未退出",
        )?;
        tick(cx).await;
    }
    Ok(())
}

fn run(scenario: Scenario) -> Result<()> {
    let fixtures = Fixtures::new()?;
    let paths = write_fixtures(&fixtures.root, scenario)?;
    let outcome = Rc::new(RefCell::new(Some("视频窗口专项未完成".to_string())));
    let finished = outcome.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx| {
            if let Err(error) = cx.open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Host),
            ) {
                *outcome.borrow_mut() = Some(error.to_string());
                cx.quit();
                return;
            }
            cx.spawn(async move |cx| {
                let lifecycle = Lifecycle::default();
                let result = exercise(paths, &lifecycle, scenario, cx).await;
                let cleanup = close_owned(&lifecycle, cx).await;
                *outcome.borrow_mut() = match (result, cleanup) {
                    (Ok(report), Ok(())) => {
                        println!("{report}");
                        None
                    }
                    (Err(error), Ok(())) | (Ok(_), Err(error)) => Some(error.to_string()),
                    (Err(error), Err(cleanup)) => Some(format!("{error}；收尾失败：{cleanup}")),
                };
                let _ = cx.update(|cx| {
                    for window in cx.windows() {
                        let _ = cx.update_window(window, |_, window, _| window.remove_window());
                    }
                    cx.quit();
                });
            })
            .detach();
        });
    let error = finished.borrow_mut().take();
    error.map_or(Ok(()), |error| Err(error.into()))
}

#[test]
#[ignore = "显式单独运行静音GPUI视频窗口检查；需交互桌面，不读取桌面像素"]
fn native_video_view_lifecycle_smoke() {
    run(Scenario::Silent).expect("视频查看窗口原生检查失败");
}

#[test]
#[ignore = "显式单独运行静音GPUI视频全屏还原及生命周期检查；需交互桌面，不读取桌面像素"]
fn native_video_view_fullscreen_restore_smoke() {
    run(Scenario::Fullscreen).expect("视频查看窗口全屏还原检查失败");
}

#[test]
#[ignore = "显式单独运行内嵌AAC的静音GPUI提交同步检查；需音频设备和交互桌面，不读取桌面像素"]
fn native_video_view_embedded_audio_submitted_smoke() {
    run(Scenario::EmbeddedAudio).expect("视频查看窗口内嵌音频提交同步检查失败");
}

#[test]
#[ignore = "显式单独运行自有视频marker屏幕ROI/QPC观测；仅内存读两块自有ROI，需交互桌面及音频设备"]
fn native_video_view_screen_roi_audio_sync_smoke() {
    screen_roi::run().expect("自有视频屏幕ROI观测失败");
}
