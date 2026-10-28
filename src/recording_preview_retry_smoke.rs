//! 独立 exact ignored GPUI 专项；只用自有 video-only 素材，不进入录制、保存或重录。
use super::*;
use crate::{
    file_snapshot::{self, FileSnapshot},
    gpui_smoke::Input,
    media_foundation::{MfRuntime, Mp4Writer, VideoReader},
    recording_preview::PlaybackShutdownProbe,
};
use futures::FutureExt;
use gpui::{Application, AsyncApp, WeakEntity};
use serde_json::{json, Value};
use std::{
    cell::Cell,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::windows::fs::MetadataExt,
    panic::AssertUnwindSafe,
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::RECT, Graphics::Gdi::MONITOR_DEFAULTTOPRIMARY,
    Media::MediaFoundation::MFVideoFormat_H264,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const FPS: u32 = 30;
const FRAMES: u32 = 90;
const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const REPORT_LIMIT: usize = 256 * 1024;
const WAIT: Duration = Duration::from_secs(5);
const UI_BUDGET: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(25);
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

fn read_owned(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure(
        metadata.is_file()
            && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0
            && metadata.len() <= FILE_LIMIT,
        "专项媒体不是有界可用普通文件",
    )?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure(
        bytes.len() as u64 == metadata.len(),
        "专项媒体读取期间长度变化",
    )?;
    Ok(bytes)
}

fn write_report(root: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    ensure(bytes.len() <= REPORT_LIMIT, "预览重试专项报告超过 256 KiB")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

struct Fixture {
    root: PathBuf,
    previous_data: Option<OsString>,
    bytes: Vec<u8>,
    original: Option<FileSnapshot>,
    auxiliary: RefCell<Option<FileSnapshot>>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::path::absolute("target/recording-preview-retry")?;
        fs::create_dir_all(&parent)?;
        let metadata = fs::symlink_metadata(&parent)?;
        ensure(
            metadata.is_dir() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
            "预览重试专项目录不可用",
        )?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        for index in 0..16 {
            let root = parent.join(format!("native-{}-{stamp}-{index}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let fixture = Self {
                        root,
                        previous_data: std::env::var_os("RUSTSHOT_DATA_DIR"),
                        bytes: Vec::new(),
                        original: None,
                        auxiliary: RefCell::new(None),
                    };
                    // 必须早于 Application、theme、Options、native_setup 和 remote_display 初始化。
                    std::env::set_var("RUSTSHOT_DATA_DIR", &fixture.root);
                    return Ok(fixture);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("预览重试专项独占目录冲突过多".into())
    }

    fn path(&self) -> PathBuf {
        self.root.join("original.mp4")
    }
    fn auxiliary_path(&self) -> PathBuf {
        self.root.join("preview-aux.mp4")
    }

    fn produce(&mut self) -> Result<Value> {
        let _runtime = MfRuntime::new()?;
        let started = Instant::now();
        ensure(!self.path().try_exists()?, "专项原片路径已存在")?;
        let mut writer = Mp4Writer::new(&self.path(), WIDTH, HEIGHT, FPS, 4_000_000, false)?;
        let mut pixels = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
        let body = (|| -> Result<()> {
            for index in 0..FRAMES {
                ensure(
                    started.elapsed() < Duration::from_secs(15),
                    "专项素材编码调用间预算耗尽",
                )?;
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        let at = ((y * WIDTH + x) * 4) as usize;
                        let shade = if x == 20 + index % 120 || y == 40 {
                            80
                        } else {
                            18
                        };
                        pixels[at..at + 4].copy_from_slice(&[shade, shade, shade, 255]);
                    }
                }
                let pts = Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(FPS));
                let end =
                    Duration::from_nanos(u64::from(index + 1) * 1_000_000_000 / u64::from(FPS));
                writer.write_video(&pixels, WIDTH, pts, end - pts)?;
            }
            Ok(())
        })();
        // 每个已创建 writer 都显式收尾一次，包括 body 错误；不运行自动回退试验。
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        match (body, finished) {
            (Err(error), Err(tail)) => return Err(format!("{error}；Finalize：{tail}").into()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Ok(()), Ok(())) => {}
        }
        self.bytes = read_owned(&self.path())?;
        ensure(!self.bytes.is_empty(), "专项原片为空")?;
        self.original = Some(file_snapshot::inspect(&self.path())?.ok_or("专项原片不存在")?);
        let mut reader = VideoReader::open(&self.path())?;
        ensure(
            reader.info.codec == MFVideoFormat_H264
                && reader.width == WIDTH
                && reader.height == HEIGHT,
            "专项原片视频类型或尺寸错误",
        )?;
        let stop = AtomicBool::new(false);
        let mut frames = 0;
        let mut last = None;
        while let Some(frame) = reader.next_cancellable(&stop)? {
            ensure(
                started.elapsed() < Duration::from_secs(15) && frames < FRAMES,
                "专项原片完整回读超预算或超帧数",
            )?;
            let expected = Duration::from_nanos(u64::from(frames) * 1_000_000_000 / u64::from(FPS));
            ensure(
                frame.bgra.len() == pixels.len()
                    && frame.time.abs_diff(expected) <= Duration::from_millis(1),
                "专项原片回读布局或 PTS 不符",
            )?;
            frames += 1;
            last = Some(frame.time);
        }
        ensure(
            frames == FRAMES && started.elapsed() < Duration::from_secs(15),
            "专项原片未在调用间预算内完整回读 90 帧至 EOF",
        )?;
        Ok(
            json!({"writer":diagnostics,"audio_requested":false,"frames":frames,"eof":true,
            "last_pts_ns":last.map(|time| time.as_nanos()),"declared_duration_ns":reader.duration.as_nanos(),
            "bytes":self.bytes.len(),"pixels":"owned dark BGRA chart; no desktop capture"}),
        )
    }

    fn repair_auxiliary(&self) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.auxiliary_path())?;
        file.write_all(&self.bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        *self.auxiliary.borrow_mut() =
            Some(file_snapshot::inspect(&self.auxiliary_path())?.ok_or("补齐的辅助预览不存在")?);
        Ok(())
    }

    fn retained(&self) -> Result<Value> {
        let original = file_snapshot::inspect(&self.path())?;
        let original_ok = original.as_ref() == self.original.as_ref()
            && self.original.is_some()
            && read_owned(&self.path())? == self.bytes;
        let expected = self.auxiliary.borrow().clone();
        let auxiliary = file_snapshot::inspect(&self.auxiliary_path())?;
        let auxiliary_ok = match expected {
            Some(expected) => {
                auxiliary.as_ref() == Some(&expected)
                    && read_owned(&self.auxiliary_path())? == self.bytes
            }
            None => auxiliary.is_none(),
        };
        Ok(
            json!({"original_snapshot_and_all_bytes_preserved":original_ok,
            "auxiliary_snapshot_and_all_bytes_preserved":auxiliary_ok,
            "auxiliary_was_created":self.auxiliary.borrow().is_some(),
            "auxiliary_ownership_registered":false}),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.previous_data {
            Some(path) => std::env::set_var("RUSTSHOT_DATA_DIR", path),
            None => std::env::remove_var("RUSTSHOT_DATA_DIR"),
        }
        // 自有原片、补齐的辅助片和报告全部保留，不删除媒体。
    }
}

struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(crate::theme::palette(cx).background))
    }
}

#[derive(Clone)]
struct PreviewOwner {
    handle: WindowHandle<RecordingPreview>,
    entity: WeakEntity<RecordingPreview>,
    native: usize,
    worker: PlaybackShutdownProbe,
}

#[derive(Default)]
struct Owner {
    host: Cell<Option<WindowHandle<Host>>>,
    host_entity: RefCell<Option<WeakEntity<Host>>>,
    host_native: Cell<usize>,
    panel: Cell<Option<WindowHandle<RecordingView>>>,
    entity: RefCell<Option<WeakEntity<RecordingView>>>,
    native: Cell<usize>,
    previews: RefCell<Vec<PreviewOwner>>,
    tracking_failed: Cell<bool>,
}

struct Observed {
    handle: WindowHandle<RecordingPreview>,
    state: PreviewPlaybackState,
    error: Option<String>,
    generation: u64,
    playing: bool,
    position: Duration,
    duration: Duration,
    rate: f32,
    pending: bool,
    has_frame: bool,
    submitted: Option<(u64, Duration)>,
    seeking: bool,
    rate_flyout: bool,
    output_ready: bool,
    controls_laid_out: bool,
}

impl Observed {
    fn value(&self) -> Value {
        json!({"handle":format!("{:?}", self.handle),"state":match self.state {
            PreviewPlaybackState::Opening=>"opening",PreviewPlaybackState::Ready=>"ready",
            PreviewPlaybackState::Unavailable=>"unavailable"},"error":self.error,
            "generation":self.generation,"playing":self.playing,"position_ns":self.position.as_nanos(),
            "duration_ns":self.duration.as_nanos(),"rate":self.rate,"pending_seek_or_step":self.pending,
            "has_frame":self.has_frame,"submitted":self.submitted.map(|(serial,time)|json!({"serial":serial,"pts_ns":time.as_nanos()})),
            "view_seeking":self.seeking,"rate_flyout":self.rate_flyout,"output_gate_ready":self.output_ready,
            "controls_laid_out":self.controls_laid_out})
    }

    fn same_transport(&self, other: &Self) -> bool {
        self.handle == other.handle
            && self.generation == other.generation
            && self.playing == other.playing
            && self.position == other.position
            && self.duration == other.duration
            && self.rate == other.rate
            && self.pending == other.pending
            && self.submitted == other.submitted
    }
}

fn track_current_preview(
    view: &RecordingView,
    owner: &Owner,
    cx: &mut Context<RecordingView>,
) -> Result<()> {
    let tracked = (|| -> Result<()> {
        let handle = view.preview.ok_or("无法登记当前真实预览及后台退出探针")?;
        if owner
            .previews
            .borrow()
            .iter()
            .any(|item| item.handle == handle)
        {
            return Ok(());
        }
        let tracked = handle.update(cx, |preview, window, cx| PreviewOwner {
            handle,
            entity: cx.entity().downgrade(),
            native: hwnd(window).0 as usize,
            worker: preview.player.shutdown_probe(),
        })?;
        ensure(
            owner.previews.borrow().len() < 2,
            "专项创建了超过两次预览，关闭证据不完整",
        )?;
        owner.previews.borrow_mut().push(tracked);
        Ok(())
    })();
    if tracked.is_err() {
        owner.tracking_failed.set(true);
    }
    tracked
}

fn observe(fixture: &Fixture, owner: &Owner, cx: &mut AsyncApp) -> Result<Observed> {
    let panel = owner.panel.get().ok_or("专项工具栏未创建")?;
    panel.update(cx, |view, window, cx| -> Result<_> {
        // 先保留当前实际后台退出探针；后面的文件/输出门失败也不能跳过清理证据。
        track_current_preview(view, owner, cx)?;
        ensure(
            hwnd(window).0 as usize == owner.native.get(),
            "专项工具栏 HWND 身份变化",
        )?;
        ensure(
            view.stage == Stage::Preview
                && view.output.as_ref() == Some(&fixture.path())
                && view.result.as_ref().is_some_and(|result| {
                    result.path == fixture.path()
                        && result.preview == fixture.auxiliary_path()
                        && result.tracks.is_empty()
                        && result.initial_mix == MixLevels::default()
                })
                && view.levels == MixLevels::default()
                && view.recording.is_none()
                && view.countdown.is_none()
                && view.audio_input_check.is_none()
                && view.mix_job.is_none()
                && view.gif_job.is_none()
                && !view.picking_save,
            "预览控件改变了成品、音轨、音量或工作槽",
        )?;
        ensure(
            view.session_files
                .writeback_request(&fixture.path())
                .is_ok(),
            "原片身份登记资格丢失",
        )?;
        let handle = view
            .preview
            .ok_or("专项必须保留真实失败/就绪 Player handle")?;
        let state = view.preview_playback_state(cx);
        let controls_laid_out = ["preview-play", "preview-rate", "preview-retry"]
            .into_iter()
            .all(|id| button_input_at(owner, window, id).is_ok());
        let observed = handle.update(cx, |preview, _, _| -> Result<_> {
            ensure(
                preview.player.audio_sources().is_empty()
                    && preview
                        .player
                        .audio_info()
                        .is_some_and(|formats| formats.is_empty())
                    && preview.player.audio_error().is_none(),
                "video-only 结果窗意外创建声音分支",
            )?;
            let (position, duration) = preview.player.position();
            Ok(Observed {
                handle,
                state,
                error: preview
                    .player
                    .error()
                    .map(|error| error.chars().take(256).collect()),
                generation: preview.player.test_transport_generation(),
                playing: preview.player.playing(),
                position,
                duration,
                rate: preview.player.rate,
                pending: preview.player.seeking(),
                has_frame: preview.has_frame(),
                submitted: preview.player.submitted_frame_point(),
                seeking: view.seeking,
                rate_flyout: view.flyout == Some(Flyout::Rate),
                output_ready: view.output_gate().ready(),
                controls_laid_out,
            })
        })??;
        Ok(observed)
    })?
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor().timer(TICK).await;
}

async fn wait(
    fixture: &Fixture,
    owner: &Owner,
    cx: &mut AsyncApp,
    started: Instant,
    predicate: impl Fn(&Observed) -> bool,
) -> Result<Observed> {
    let wait_started = Instant::now();
    let mut last = None;
    while wait_started.elapsed() < WAIT && started.elapsed() < UI_BUDGET {
        let observed = observe(fixture, owner, cx)?;
        if predicate(&observed) {
            return Ok(observed);
        }
        last = Some(observed.value());
        tick(cx).await;
    }
    Err(format!("预览重试专项条件未在调用间预算内满足：last={last:?}").into())
}

fn input_point(owner: &Owner, id: &'static str, cx: &mut AsyncApp) -> Result<(Input, (f32, f32))> {
    owner
        .panel
        .get()
        .ok_or("专项工具栏不存在")?
        .update(cx, |_, window, _| button_input_at(owner, window, id))?
}

fn button_input_at(
    owner: &Owner,
    window: &Window,
    id: &'static str,
) -> Result<(Input, (f32, f32))> {
    let bounds = button_bounds(id).ok_or("当前帧没有目标按钮边界")?;
    ensure(
        f32::from(bounds.size.width) > 0. && f32::from(bounds.size.height) > 0.,
        "目标按钮尚未布局",
    )?;
    let point = (
        f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.,
        f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.,
    );
    input_at(owner, window, point)
}

fn input_at(owner: &Owner, window: &Window, point: (f32, f32)) -> Result<(Input, (f32, f32))> {
    let h = hwnd(window);
    ensure(
        h.0 as usize == owner.native.get() && unsafe { IsWindow(Some(h)) }.as_bool(),
        "输入目标不是自有工具栏",
    )?;
    let scale = window.scale_factor();
    let mut client = RECT::default();
    unsafe { GetClientRect(h, &mut client) }?;
    ensure(
        scale.is_finite()
            && scale > 0.
            && point.0.is_finite()
            && point.1.is_finite()
            && point.0 > 0.
            && point.1 > 0.
            && point.0 * scale < client.right as f32
            && point.1 * scale < client.bottom as f32,
        "输入点不在当前自有 HWND 客户区",
    )?;
    Ok((
        Input {
            handle: owner.native.get(),
            scale,
        },
        point,
    ))
}

fn click(owner: &Owner, id: &'static str, cx: &mut AsyncApp) -> Result<()> {
    let (input, point) = input_point(owner, id, cx)?;
    input.click(point);
    Ok(())
}

fn seek_drag(owner: &Owner, cx: &mut AsyncApp) -> Result<()> {
    let (input, a, b) = owner.panel.get().ok_or("专项工具栏不存在")?.update(
        cx,
        |view, window, _| -> Result<_> {
            let top = if view.flyout_above {
                view.extra_height()
            } else {
                0.
            };
            let y = top + 1. + SEEK_HEIGHT / 2.;
            let x = |ratio| 12. + (view.width() - 24.) * ratio;
            let (input, a) = input_at(owner, window, (x(0.15), y))?;
            let (_, b) = input_at(owner, window, (x(0.65), y))?;
            Ok((input, a, b))
        },
    )??;
    // Input.click 接受 DIP，drag 接受物理整数；drag 自身带 SMOKE_MOVE，不移动真实鼠标。
    let physical = |(x, y): (f32, f32)| {
        (
            (x * input.scale).round() as u32,
            (y * input.scale).round() as u32,
        )
    };
    input.drag(physical(a), physical(b));
    Ok(())
}

fn click_rate_option(owner: &Owner, index: usize, cx: &mut AsyncApp) -> Result<()> {
    let (input, point) = owner.panel.get().ok_or("专项工具栏不存在")?.update(
        cx,
        |view, window, _| -> Result<_> {
            ensure(view.flyout == Some(Flyout::Rate), "速度面板尚未打开")?;
            let top = if view.flyout_above {
                0.
            } else {
                view.bar_height() + 4.
            };
            // option_row 当前真实布局：面板内边距4+边框1，每行28 DIP。
            input_at(
                owner,
                window,
                (
                    view.flyout_left(Flyout::Rate) + view.flyout_width(Flyout::Rate) / 2.,
                    top + 5. + 28. * index as f32 + 14.,
                ),
            )
        },
    )??;
    input.click(point);
    Ok(())
}

fn region() -> Result<ScreenRect> {
    let mut monitor = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(
            MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY),
            &mut monitor,
        )
        .ok()?;
    }
    ensure(
        monitor.rcWork.right - monitor.rcWork.left >= 1000
            && monitor.rcWork.bottom - monitor.rcWork.top >= 600,
        "工作区不足以放置自有预览工具栏",
    )?;
    Ok(ScreenRect {
        left: monitor.rcWork.left + 40,
        top: monitor.rcWork.top + 80,
        width: WIDTH,
        height: HEIGHT,
    })
}

async fn exercise(
    fixture: &Fixture,
    owner: &Owner,
    cx: &mut AsyncApp,
    report: &Rc<RefCell<Value>>,
) -> Result<()> {
    let started = Instant::now();
    cx.update(|cx| -> Result<()> {
        let panel = RecordingView::open_with(region()?, false, cx)?;
        owner.panel.set(Some(panel));
        panel.update(cx, |view, window, cx| -> Result<()> {
            owner.native.set(hwnd(window).0 as usize);
            *owner.entity.borrow_mut() = Some(cx.entity().downgrade());
            crate::gpui_smoke::isolate_input(hwnd(window))?;
            ensure(
                view.stage == Stage::Idle
                    && view.recording.is_none()
                    && view.countdown.is_none()
                    && view.audio_input_check.is_none()
                    && view.drawing.is_none()
                    && !view.options.microphone
                    && !view.options.system_audio,
                "初始化进入了录制或采集分支",
            )?;
            view.finish_recording(
                Ok(RecordingResult {
                    path: fixture.path(),
                    preview: fixture.auxiliary_path(),
                    width: WIDTH,
                    height: HEIGHT,
                    duration: Duration::from_secs(3),
                    preset: Preset::CompactMp4,
                    bytes: fixture.bytes.len() as u64,
                    tracks: Vec::new(),
                    initial_mix: MixLevels::default(),
                    encoding: None,
                }),
                window,
                cx,
            );
            cx.notify();
            Ok(())
        })??;
        Ok(())
    })??;
    let unavailable = wait(fixture, owner, cx, started, |observed| {
        observed.state == PreviewPlaybackState::Unavailable
            && observed.error.is_some()
            && observed.controls_laid_out
    })
    .await?;
    ensure(unavailable.output_ready, "失败预览不应阻止保存原片")?;
    report.borrow_mut()["unavailable"] = unavailable.value();
    for control in ["preview-play", "preview-rate", "seek"] {
        if control == "seek" {
            seek_drag(owner, cx)?;
        } else {
            click(owner, control, cx)?;
        }
        cx.background_executor()
            .timer(Duration::from_millis(150))
            .await;
        let after = observe(fixture, owner, cx)?;
        report.borrow_mut()[format!("unavailable_{control}")] = after.value();
        ensure(
            after.state == PreviewPlaybackState::Unavailable
                && after.error.is_some()
                && after.same_transport(&unavailable)
                && !after.seeking
                && !after.rate_flyout
                && after.output_ready,
            "未就绪控件派发了 transport、留下手势或阻塞原片输出",
        )?;
    }
    fixture.repair_auxiliary()?;
    click(owner, "preview-retry", cx)?;
    let ready = wait(fixture, owner, cx, started, |observed| {
        observed.handle != unavailable.handle
            && observed.state == PreviewPlaybackState::Ready
            && observed.error.is_none()
            && observed.has_frame
            && observed.submitted.is_some()
    })
    .await?;
    ensure(
        ready.output_ready
            && ready.duration.abs_diff(Duration::from_secs(3)) <= Duration::from_millis(1),
        "重试后原片输出门或真实时长无效",
    )?;
    report.borrow_mut()["retry_ready"] = ready.value();
    let paused = if ready.playing {
        click(owner, "preview-play", cx)?;
        wait(fixture, owner, cx, started, |observed| {
            !observed.playing && observed.generation > ready.generation
        })
        .await?
    } else {
        ready
    };
    click(owner, "preview-play", cx)?;
    let playing = wait(fixture, owner, cx, started, |observed| {
        observed.playing
            && observed.generation > paused.generation
            && observed.submitted.is_some()
            && observed.submitted != paused.submitted
    })
    .await?;
    report.borrow_mut()["ready_play"] = playing.value();
    ensure(
        playing.output_ready && !playing.seeking,
        "播放后原片输出门被意外手势阻塞",
    )?;
    click(owner, "preview-play", cx)?;
    let paused = wait(fixture, owner, cx, started, |observed| {
        !observed.playing && observed.generation > playing.generation
    })
    .await?;
    click(owner, "preview-rate", cx)?;
    wait(fixture, owner, cx, started, |observed| observed.rate_flyout).await?;
    // 等一次生产50ms定位轮询，随后依当前flyout几何发实际选项点击。
    cx.background_executor()
        .timer(Duration::from_millis(100))
        .await;
    let index = RATES
        .iter()
        .position(|rate| (*rate - paused.rate).abs() > 0.01)
        .ok_or("没有不同的测试播放速度")?;
    click_rate_option(owner, index, cx)?;
    let rated = wait(fixture, owner, cx, started, |observed| {
        (observed.rate - RATES[index]).abs() < 0.01
            && observed.generation > paused.generation
            && !observed.rate_flyout
    })
    .await?;
    report.borrow_mut()["ready_rate"] = rated.value();
    ensure(
        rated.output_ready && !rated.seeking,
        "速度选择后原片输出门被意外手势阻塞",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(100))
        .await;
    seek_drag(owner, cx)?;
    let seeked = wait(fixture, owner, cx, started, |observed| {
        let target = observed.duration.mul_f32(0.65);
        observed.generation > rated.generation
            && !observed.pending
            && !observed.seeking
            && observed.has_frame
            && observed.position.abs_diff(target) <= Duration::from_millis(100)
            && observed.submitted.is_some_and(|point| {
                Some(point) != rated.submitted
                    && point.1.abs_diff(target) <= Duration::from_millis(100)
            })
    })
    .await?;
    ensure(seeked.output_ready, "真实Seek结束后原片输出门仍被手势阻塞")?;
    report.borrow_mut()["ready_seek"] = seeked.value();
    Ok(())
}

fn released(owner: &Owner) -> bool {
    let panel_gone = owner
        .entity
        .borrow()
        .as_ref()
        .is_none_or(|entity| entity.upgrade().is_none());
    let native_gone = owner.native.get() == 0
        || !unsafe { IsWindow(Some(HWND(owner.native.get() as *mut _))) }.as_bool();
    !owner.tracking_failed.get()
        && panel_gone
        && native_gone
        && owner.previews.borrow().iter().all(|preview| {
            preview.entity.upgrade().is_none()
                && preview.worker.finished()
                && !unsafe { IsWindow(Some(HWND(preview.native as *mut _))) }.as_bool()
        })
}

async fn close_owned(owner: &Owner, cx: &mut AsyncApp) -> Result<Value> {
    let tracking_error = if let Some(panel) = owner.panel.get() {
        let captured = panel.update(cx, |view, window, cx| {
            // 首次 observe 或 Retry 后资格检查可能先失败；移除窗口前仍须捕获当前后台。
            let captured = track_current_preview(view, owner, cx);
            view.close_preview(cx);
            view.border.take();
            view.tip.take();
            view.toast.take();
            window.remove_window();
            captured
        });
        match captured {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => {
                owner.tracking_failed.set(true);
                Some(error.to_string())
            }
        }
    } else {
        owner.tracking_failed.set(true);
        Some("无法读取自有工具栏，当前播放器清理证据未知".to_owned())
    };
    for preview in owner.previews.borrow().clone() {
        let _ = preview
            .handle
            .update(cx, |_, window, _| window.remove_window());
    }
    let started = Instant::now();
    while started.elapsed() < WAIT {
        if released(owner) {
            return Ok(
                json!({"owned_panel_and_all_preview_hwnds_entities_released":true,
                "all_tracked_video_workers_exited":owner.previews.borrow().iter().all(|preview|preview.worker.video_exited()),
                "tracked_preview_count":owner.previews.borrow().len(),
                "shutdown_probes_finished":true,"method":"explicit test cleanup; product Done/save/rerecord not exercised"}),
            );
        }
        tick(cx).await;
    }
    Err(
        format!("专项显式清理后仍有自有资源存活或当前后台证据未知：tracking={tracking_error:?}")
            .into(),
    )
}

fn run() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let report = Rc::new(RefCell::new(
        json!({"ok":false,"mode":"missing_auxiliary_retry",
        "scope":["Dedicated exact GPUI process with a fresh local RUSTSHOT_DATA_DIR set before Application/theme/options/native initialization, restored only after Application exits. System theme; no desktop pixels, camera, microphone, user configuration/history/clipboard, save dialog or rerecord action.",
            "Production finish_recording opens a real failed Player for an absent auxiliary path. The owned original remains registered; the later-created auxiliary never obtains OwnedFile cleanup ownership. Input is targeted posted mouse messages to owned HWNDs, not physical user input.",
            "Unavailable controls require unchanged real transport generation; Ready positive controls require command generations and actual submitted frames/settled seeking. Space is untested because active-window, focus and key-owner gates are deliberately not bypassed.",
            "Short cooperative waits cannot interrupt a blocking native media/I/O call. Explicit test cleanup is not evidence of product Done/save/rerecord cleanup behavior. All owned media and actual reports are retained."],
        "space":{"tested":false,"reason":"No activation/focus/key-owner bypass; no synthetic key is claimed as a user key."}}),
    ));
    let produced = std::panic::catch_unwind(AssertUnwindSafe(|| fixture.produce()));
    match produced {
        Ok(Ok(value)) => report.borrow_mut()["fixture"] = value,
        Ok(Err(error)) => {
            report.borrow_mut()["fixture_error"] = error
                .to_string()
                .chars()
                .take(2048)
                .collect::<String>()
                .into();
            write_report(&fixture.root, &report.borrow())?;
            return Err(error);
        }
        Err(_) => {
            report.borrow_mut()["fixture_error"] = "素材生成捕获panic".into();
            write_report(&fixture.root, &report.borrow())?;
            return Err("预览重试专项素材生成panic".into());
        }
    }
    let fixture = Rc::new(fixture);
    let owner = Rc::new(Owner::default());
    let input_fixture = fixture.clone();
    let app_owner = owner.clone();
    let app_report = report.clone();
    let outcome = Rc::new(RefCell::new(Some("GPUI专项未完成".to_owned())));
    let app_outcome = outcome.clone();
    let application = std::panic::catch_unwind(AssertUnwindSafe(|| {
        Application::new()
            .with_assets(crate::toolbar_icons::IconAssets)
            .run(move |cx| {
                crate::theme::install(cx);
                match cx.open_window(
                    WindowOptions {
                        show: false,
                        focus: false,
                        ..Default::default()
                    },
                    |window, cx| {
                        app_owner.host_native.set(hwnd(window).0 as usize);
                        cx.new(|cx| {
                            *app_owner.host_entity.borrow_mut() = Some(cx.entity().downgrade());
                            Host
                        })
                    },
                ) {
                    Ok(host) => app_owner.host.set(Some(host)),
                    Err(error) => {
                        *app_outcome.borrow_mut() = Some(error.to_string());
                        cx.quit();
                        return;
                    }
                }
                cx.spawn(async move |cx| {
                    let result =
                        AssertUnwindSafe(exercise(&input_fixture, &app_owner, cx, &app_report))
                            .catch_unwind()
                            .await;
                    let body_error = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("专项交互捕获panic".to_owned()),
                    };
                    let cleanup = AssertUnwindSafe(close_owned(&app_owner, cx))
                        .catch_unwind()
                        .await;
                    let cleanup_error = match cleanup {
                        Ok(Ok(value)) => {
                            app_report.borrow_mut()["cleanup"] = value;
                            None
                        }
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("专项窗口清理捕获panic".to_owned()),
                    };
                    app_report.borrow_mut()["interaction_completed"] = body_error.is_none().into();
                    if let Some(error) = &body_error {
                        app_report.borrow_mut()["interaction_error"] =
                            error.chars().take(2048).collect::<String>().into();
                    }
                    if let Some(error) = &cleanup_error {
                        app_report.borrow_mut()["cleanup_error"] =
                            error.chars().take(2048).collect::<String>().into();
                    }
                    *app_outcome.borrow_mut() = body_error.or(cleanup_error);
                    let _ = cx.update(|cx| {
                        if let Some(host) = app_owner.host.get() {
                            let _ = host.update(cx, |_, window, _| window.remove_window());
                        }
                        cx.quit();
                    });
                })
                .detach();
            });
    }));
    if application.is_err() {
        report.borrow_mut()["application_error"] =
            "Application捕获panic，关闭状态不得计成功".into();
    }
    let retained = fixture.retained();
    let retained_ok = match retained {
        Ok(value) => {
            let ok = value["original_snapshot_and_all_bytes_preserved"] == true
                && value["auxiliary_snapshot_and_all_bytes_preserved"] == true
                && value["auxiliary_was_created"] == true;
            report.borrow_mut()["retained"] = value;
            ok
        }
        Err(error) => {
            report.borrow_mut()["retention_error"] = error
                .to_string()
                .chars()
                .take(2048)
                .collect::<String>()
                .into();
            false
        }
    };
    let host_released = owner
        .host_entity
        .borrow()
        .as_ref()
        .is_none_or(|entity| entity.upgrade().is_none())
        && (owner.host_native.get() == 0
            || !unsafe { IsWindow(Some(HWND(owner.host_native.get() as *mut _))) }.as_bool());
    let resources_released = released(&owner) && host_released;
    report.borrow_mut()["all_owned_resources_released_after_application"] =
        resources_released.into();
    if let Some(error) = outcome.borrow().as_ref() {
        report.borrow_mut()["run_error"] = error.chars().take(2048).collect::<String>().into();
    }
    let ok = application.is_ok() && outcome.borrow().is_none() && resources_released && retained_ok;
    report.borrow_mut()["ok"] = ok.into();
    report.borrow_mut()["environment_restore"] =
        "RUSTSHOT_DATA_DIR restored by Fixture Drop after this report and Application exit".into();
    write_report(&fixture.root, &report.borrow())?;
    if ok {
        Ok(())
    } else {
        Err(format!(
            "预览重试专项未通过；报告：{}",
            fixture.root.join("report.json").display()
        )
        .into())
    }
}

#[test]
#[ignore = "独立 exact GPUI 专项；自有无音轨素材，真实失败→补aux→鼠标Retry，不采桌面/摄像头/麦克风、不激活窗口"]
fn native_recording_preview_missing_aux_retry_smoke() -> Result<()> {
    run()
}
