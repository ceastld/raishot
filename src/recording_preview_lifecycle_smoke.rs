//! 独立 exact ignored 专项：真实 Done/重录按钮的文件来源与删除资格。
//! 只提交自有已收尾结果；从不点 Record，不采集桌面、设备或声音。
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
const FRAMES: u32 = 12;
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
        "专项文件不是有界普通本地文件",
    )?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure(
        bytes.len() as u64 == metadata.len(),
        "专项文件读取期间长度变化",
    )?;
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn snapshot(path: &Path) -> Result<FileSnapshot> {
    file_snapshot::inspect(path)?.ok_or_else(|| "专项自有文件不存在".into())
}

fn same_identity(left: &FileSnapshot, right: &FileSnapshot) -> bool {
    left.volume == right.volume && left.id == right.id && left.created == right.created
}

fn snapshot_value(value: &FileSnapshot) -> Value {
    json!({"volume":value.volume,"id":value.id,"created":value.created,
        "written":value.written,"bytes":value.bytes,"attributes":value.attributes})
}

fn write_report(root: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    ensure(bytes.len() <= REPORT_LIMIT, "来源专项报告超过 256 KiB")?;
    write_new(&root.join("report.json"), &bytes)
}

struct Fixture {
    root: PathBuf,
    previous_data: Option<OsString>,
    bytes: Vec<u8>,
    baseline: Option<FileSnapshot>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::path::absolute("target/recording-preview-lifecycle")?;
        fs::create_dir_all(&parent)?;
        let metadata = fs::symlink_metadata(&parent)?;
        ensure(
            metadata.is_dir() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
            "来源专项目录不可用",
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
                        baseline: None,
                    };
                    // 早于 MF、Application、theme、options 及原生窗口初始化。
                    std::env::set_var("RUSTSHOT_DATA_DIR", &fixture.root);
                    return Ok(fixture);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("来源专项独占目录冲突过多".into())
    }

    fn produce(&mut self) -> Result<Value> {
        let _runtime = MfRuntime::new()?;
        let path = self.root.join("baseline.mp4");
        ensure(!path.try_exists()?, "基准路径已存在")?;
        let started = Instant::now();
        let mut writer = Mp4Writer::new(&path, WIDTH, HEIGHT, FPS, 4_000_000, false)?;
        let mut pixels = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
        let body = (|| -> Result<()> {
            for index in 0..FRAMES {
                ensure(
                    started.elapsed() < Duration::from_secs(15),
                    "基准编码调用间预算耗尽",
                )?;
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        let at = ((y * WIDTH + x) * 4) as usize;
                        let shade = if x == 20 + index || y == 40 { 80 } else { 18 };
                        pixels[at..at + 4].copy_from_slice(&[shade, shade, shade, 255]);
                    }
                }
                let time = frame_time(index);
                writer.write_video(&pixels, WIDTH, time, frame_time(index + 1) - time)?;
            }
            Ok(())
        })();
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        match (body, finished) {
            (Err(error), Err(tail)) => return Err(format!("{error}；Finalize：{tail}").into()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Ok(()), Ok(())) => {}
        }
        self.bytes = read_owned(&path)?;
        ensure(!self.bytes.is_empty(), "完整基准为空")?;
        self.baseline = Some(snapshot(&path)?);
        let mut reader = VideoReader::open(&path)?;
        ensure(
            reader.info.codec == MFVideoFormat_H264
                && reader.width == WIDTH
                && reader.height == HEIGHT,
            "基准类型或尺寸不符",
        )?;
        let stop = AtomicBool::new(false);
        let mut count = 0;
        while let Some(frame) = reader.next_cancellable(&stop)? {
            ensure(
                count < FRAMES && started.elapsed() < Duration::from_secs(15),
                "基准完整回读超限",
            )?;
            ensure(
                frame.bgra.len() == pixels.len()
                    && frame.time.abs_diff(frame_time(count)) <= Duration::from_millis(1),
                "基准完整回读布局或 PTS 不符",
            )?;
            count += 1;
        }
        ensure(count == FRAMES, "基准没有完整回读至 EOF")?;
        Ok(
            json!({"writer":diagnostics,"audio_requested":false,"frames":count,"eof":true,
            "bytes":self.bytes.len(),"pts_tolerance_ns":1_000_000}),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.previous_data {
            Some(path) => std::env::set_var("RUSTSHOT_DATA_DIR", path),
            None => std::env::remove_var("RUSTSHOT_DATA_DIR"),
        }
        // 不补删：生产动作以外的自有媒体、备份、失败产物和报告全部保留。
    }
}

fn frame_time(index: u32) -> Duration {
    Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(FPS))
}

#[derive(Clone, Copy)]
enum Action {
    Rerecord,
    Done,
}
impl Action {
    fn button(self) -> &'static str {
        match self {
            Self::Rerecord => "preview-rerecord",
            Self::Done => "preview-close",
        }
    }
}

#[derive(Clone)]
struct Case {
    name: &'static str,
    action: Action,
    replaced: bool,
    original: PathBuf,
    auxiliary: PathBuf,
    original_before: FileSnapshot,
    auxiliary_before: FileSnapshot,
}

impl Case {
    fn create(
        fixture: &Fixture,
        name: &'static str,
        action: Action,
        replaced: bool,
    ) -> Result<Self> {
        let directory = fixture.root.join(name);
        fs::create_dir(&directory)?;
        let original = directory.join("original.mp4");
        let auxiliary = directory.join("preview-aux.mp4");
        write_new(&original, &fixture.bytes)?;
        write_new(&auxiliary, &fixture.bytes)?;
        Ok(Self {
            name,
            action,
            replaced,
            original_before: snapshot(&original)?,
            auxiliary_before: snapshot(&auxiliary)?,
            original,
            auxiliary,
        })
    }

    fn result(&self, bytes: usize) -> RecordingResult {
        RecordingResult {
            path: self.original.clone(),
            preview: self.auxiliary.clone(),
            width: WIDTH,
            height: HEIGHT,
            duration: frame_time(FRAMES),
            preset: Preset::CompactMp4,
            bytes: bytes as u64,
            tracks: Vec::new(),
            initial_mix: MixLevels::default(),
            encoding: None,
        }
    }

    fn replace(&self, fixture: &Fixture) -> Result<Value> {
        let mut evidence = Vec::new();
        for (path, before) in [
            (&self.original, &self.original_before),
            (&self.auxiliary, &self.auxiliary_before),
        ] {
            ensure(
                snapshot(path)? == *before && read_owned(path)? == fixture.bytes,
                "替换前原对象已变化",
            )?;
            let backup = path.with_extension("registered-backup.mp4");
            ensure(!backup.try_exists()?, "替换备份已存在")?;
            fs::rename(path, &backup)?;
            write_new(path, &fixture.bytes)?;
            let replacement = snapshot(path)?;
            ensure(
                !same_identity(before, &replacement)
                    && snapshot(&backup)? == *before
                    && read_owned(&backup)? == fixture.bytes
                    && read_owned(path)? == fixture.bytes,
                "替换未保留原备份或没有新文件身份",
            )?;
            evidence.push(json!({"path":path,"registered":snapshot_value(before),
                "replacement":snapshot_value(&replacement),"backup":backup}));
        }
        Ok(json!(evidence))
    }

    fn file_gate(&self, fixture: &Fixture, replacements: &Value) -> Result<Value> {
        let mut rows = Vec::new();
        for (index, (path, before)) in [
            (&self.original, &self.original_before),
            (&self.auxiliary, &self.auxiliary_before),
        ]
        .into_iter()
        .enumerate()
        {
            let after = file_snapshot::inspect(path)?;
            if self.replaced {
                let expected = &replacements[index]["replacement"];
                let actual = after.as_ref().map(snapshot_value).unwrap_or(Value::Null);
                let backup = path.with_extension("registered-backup.mp4");
                ensure(
                    actual == *expected
                        && read_owned(path)? == fixture.bytes
                        && snapshot(&backup)? == *before
                        && read_owned(&backup)? == fixture.bytes,
                    "生产动作删除或改变了外部替换物/原备份",
                )?;
                rows.push(json!({"path":path,"replacement_preserved":true,"registered_backup_preserved":true,
                    "registered_backup_snapshot":snapshot_value(before),"snapshot":actual}));
            } else {
                let keep = index == 0 && matches!(self.action, Action::Done);
                if keep {
                    ensure(
                        after.as_ref() == Some(before) && read_owned(path)? == fixture.bytes,
                        "Done改变或删除了原片",
                    )?;
                } else {
                    ensure(
                        after.is_none(),
                        "生产动作没有删除仍属本会话的原片/aux；不补删或放宽通过",
                    )?;
                }
                rows.push(json!({"path":path,"expected_retained":keep,"exists":after.is_some(),
                    "registered_snapshot":snapshot_value(before),"after":after.as_ref().map(snapshot_value)}));
            }
        }
        Ok(json!(rows))
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
struct PreviewTrack {
    handle: WindowHandle<RecordingPreview>,
    entity: WeakEntity<RecordingPreview>,
    native: usize,
    worker: PlaybackShutdownProbe,
}
struct PanelTrack {
    handle: WindowHandle<RecordingView>,
    entity: Option<WeakEntity<RecordingView>>,
    native: usize,
    baseline: Option<(
        ScreenRect,
        RecordingOptions,
        EncoderPolicy,
        Option<QualityProfile>,
    )>,
}
#[derive(Default)]
struct Owner {
    panels: RefCell<Vec<Rc<RefCell<PanelTrack>>>>,
    previews: RefCell<Vec<PreviewTrack>>,
    tracking_failed: Cell<bool>,
    host: Cell<Option<WindowHandle<Host>>>,
    host_entity: RefCell<Option<WeakEntity<Host>>>,
    host_native: Cell<usize>,
}

fn track_preview(
    view: &RecordingView,
    owner: &Owner,
    cx: &mut Context<RecordingView>,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let handle = view.preview.ok_or("预览探针缺少真实handle")?;
        if owner
            .previews
            .borrow()
            .iter()
            .any(|item| item.handle == handle)
        {
            return Ok(());
        }
        let tracked = handle.update(cx, |preview, window, cx| PreviewTrack {
            handle,
            entity: cx.entity().downgrade(),
            native: hwnd(window).0 as usize,
            worker: preview.player.shutdown_probe(),
        })?;
        ensure(owner.previews.borrow().len() < 6, "预览代数超过固定六代")?;
        owner.previews.borrow_mut().push(tracked);
        Ok(())
    })();
    if result.is_err() {
        owner.tracking_failed.set(true);
    }
    result
}

fn unchanged(view: &RecordingView, panel: &PanelTrack) -> Result<()> {
    let baseline = panel.baseline.as_ref().ok_or("工具栏初始状态未登记")?;
    ensure(
        view.region == baseline.0
            && view.options == baseline.1
            && view.encoder_policy == baseline.2
            && view.quality_profile == baseline.3,
        "专项改变了选区、录制选项或内部编码策略",
    )?;
    ensure(
        view.recording.is_none()
            && view.countdown.is_none()
            && view.abandoned.is_none()
            && view.audio_input_check.is_none()
            && view.mix_job.is_none()
            && view.gif_job.is_none()
            && view.drawing.is_none()
            && !view.picking_save
            && !view.closing,
        "专项意外进入录制/倒计时/设备/输出工作槽",
    )
}

fn panel_gone(panel: &PanelTrack) -> bool {
    panel
        .entity
        .as_ref()
        .is_some_and(|entity| entity.upgrade().is_none())
        && panel.native != 0
        && !unsafe { IsWindow(Some(HWND(panel.native as *mut _))) }.as_bool()
}
fn previews_gone(owner: &Owner) -> bool {
    !owner.tracking_failed.get()
        && !owner.previews.borrow().is_empty()
        && owner.previews.borrow().iter().all(|item| {
            item.entity.upgrade().is_none()
                && item.worker.finished()
                && !unsafe { IsWindow(Some(HWND(item.native as *mut _))) }.as_bool()
        })
}
fn released(owner: &Owner) -> bool {
    !owner.panels.borrow().is_empty()
        && owner
            .panels
            .borrow()
            .iter()
            .all(|panel| panel_gone(&panel.borrow()))
        && previews_gone(owner)
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor().timer(TICK).await;
}
async fn wait(
    cx: &mut AsyncApp,
    started: Instant,
    mut predicate: impl FnMut(&mut AsyncApp) -> Result<bool>,
) -> Result<()> {
    let began = Instant::now();
    while began.elapsed() < WAIT && started.elapsed() < UI_BUDGET {
        if predicate(cx)? {
            return Ok(());
        }
        tick(cx).await;
    }
    Err("专项条件未在调用间预算内满足".into())
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
        "工作区不足以放置专项工具栏",
    )?;
    Ok(ScreenRect {
        left: monitor.rcWork.left + 40,
        top: monitor.rcWork.top + 80,
        width: WIDTH,
        height: HEIGHT,
    })
}

fn open_panel(owner: &Owner, cx: &mut AsyncApp) -> Result<Rc<RefCell<PanelTrack>>> {
    cx.update(|cx| -> Result<_> {
        ensure(owner.panels.borrow().len() < 2, "工具栏超过固定两个")?;
        let handle = RecordingView::open_with(region()?, false, cx)?;
        // 成功创建即先登记handle，后续原生隔离/断言失败也可回收。
        let panel = Rc::new(RefCell::new(PanelTrack {
            handle,
            entity: None,
            native: 0,
            baseline: None,
        }));
        owner.panels.borrow_mut().push(panel.clone());
        handle.update(cx, |view, window, cx| -> Result<()> {
            panel.borrow_mut().entity = Some(cx.entity().downgrade());
            panel.borrow_mut().native = hwnd(window).0 as usize;
            panel.borrow_mut().baseline = Some((
                view.region,
                view.options.clone(),
                view.encoder_policy,
                view.quality_profile,
            ));
            crate::gpui_smoke::isolate_input(hwnd(window))?;
            unchanged(view, &panel.borrow())?;
            ensure(
                view.stage == Stage::Idle && !view.options.microphone && !view.options.system_audio,
                "初始工具栏不是无设备的Idle",
            )?;
            Ok(())
        })??;
        Ok(panel)
    })?
}

fn button_input(
    panel: &PanelTrack,
    window: &Window,
    id: &'static str,
) -> Result<(Input, (f32, f32))> {
    let h = hwnd(window);
    ensure(
        h.0 as usize == panel.native && unsafe { IsWindow(Some(h)) }.as_bool(),
        "按钮不属于自有HWND",
    )?;
    let bounds = button_bounds(id).ok_or("实际按钮尚未布局")?;
    let point = (
        f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.,
        f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.,
    );
    let scale = window.scale_factor();
    let mut client = RECT::default();
    unsafe { GetClientRect(h, &mut client) }?;
    ensure(
        f32::from(bounds.size.width) > 0.
            && f32::from(bounds.size.height) > 0.
            && scale.is_finite()
            && scale > 0.
            && point.0.is_finite()
            && point.1.is_finite()
            && point.0 > 0.
            && point.1 > 0.
            && point.0 * scale < client.right as f32
            && point.1 * scale < client.bottom as f32,
        "按钮布局不在自有客户区",
    )?;
    Ok((
        Input {
            handle: panel.native,
            scale,
        },
        point,
    ))
}

fn ready(case: &Case, panel: &PanelTrack, owner: &Owner, cx: &mut AsyncApp) -> Result<bool> {
    panel
        .handle
        .update(cx, |view, window, cx| -> Result<bool> {
            track_preview(view, owner, cx)?;
            unchanged(view, panel)?;
            ensure(
                view.stage == Stage::Preview
                    && view.output.as_ref() == Some(&case.original)
                    && view.result.as_ref().is_some_and(|result| {
                        result.path == case.original
                            && result.preview == case.auxiliary
                            && result.tracks.is_empty()
                    }),
                "结果来源发生变化",
            )?;
            ensure(
                view.session_files.writeback_request(&case.original).is_ok() != case.replaced,
                "替换前/后的原件写回资格不符",
            )?;
            if view.preview_playback_state(cx) != PreviewPlaybackState::Ready {
                return Ok(false);
            }
            if button_input(panel, window, case.action.button()).is_err() {
                return Ok(false);
            }
            let handle = view.preview.ok_or("Ready缺少实际预览")?;
            handle.update(cx, |preview, _, _| -> Result<bool> {
                ensure(
                    preview.player.audio_sources().is_empty()
                        && preview
                            .player
                            .audio_info()
                            .is_some_and(|formats| formats.is_empty())
                        && preview.player.audio_error().is_none(),
                    "专项意外创建音频后台",
                )?;
                ensure(preview.player.error().is_none(), "专项就绪预览出现解码错误")?;
                Ok(preview.has_frame() && preview.player.submitted_frame_point().is_some())
            })?
        })?
}

fn mark(report: &Rc<RefCell<Value>>, case: &Case, stage: &str) {
    report.borrow_mut()["stage"] = format!("{}:{stage}", case.name).into();
}

async fn exercise_case(
    fixture: &Fixture,
    case: &Case,
    panel: &Rc<RefCell<PanelTrack>>,
    owner: &Owner,
    cx: &mut AsyncApp,
    report: &Rc<RefCell<Value>>,
    started: Instant,
) -> Result<()> {
    mark(report, case, "finish_recording_registers_source_and_aux");
    let handle = panel.borrow().handle;
    handle.update(cx, |view, window, cx| -> Result<()> {
        unchanged(view, &panel.borrow())?;
        ensure(
            view.stage == Stage::Idle
                && view.result.is_none()
                && view.output.is_none()
                && view.preview.is_none(),
            "复用工具栏前结果状态未撤销",
        )?;
        // 复用工具栏时先丢弃旧按钮坐标，等待本轮 Preview 的真实布局后才能投递输入。
        BUTTON_BOUNDS.with(|bounds| bounds.borrow_mut().clear());
        view.finish_recording(Ok(case.result(fixture.bytes.len())), window, cx);
        track_preview(view, owner, cx)?;
        cx.notify();
        Ok(())
    })??;
    // 初次登记先以原始身份观察；replaced标记在真正换名之后才用于资格门。
    mark(report, case, "wait_initial_ready_and_layout");
    let mut initial = case.clone();
    initial.replaced = false;
    wait(cx, started, |cx| {
        ready(&initial, &panel.borrow(), owner, cx)
    })
    .await?;
    let replacements = if case.replaced {
        mark(
            report,
            case,
            "close_reader_before_owned_external_replacement",
        );
        handle.update(cx, |view, _, cx| -> Result<()> {
            track_preview(view, owner, cx)?;
            view.close_preview(cx);
            cx.notify();
            Ok(())
        })??;
        wait(cx, started, |_| Ok(previews_gone(owner))).await?;
        mark(
            report,
            case,
            "replace_only_after_registration_without_readoption",
        );
        let evidence = case.replace(fixture)?;
        report.borrow_mut()["cases"][case.name]["replacement"] = evidence.clone();
        handle.update(cx, |view, window, cx| -> Result<()> {
            ensure(
                view.session_files
                    .writeback_request(&case.original)
                    .is_err(),
                "替换物被错误重新认领",
            )?;
            let result = view.result.clone().ok_or("替换准备丢失原结果")?;
            BUTTON_BOUNDS.with(|bounds| bounds.borrow_mut().clear());
            ensure(
                view.open_result_preview(&result, window, cx),
                "替换后未重新打开预览",
            )?;
            track_preview(view, owner, cx)?;
            cx.notify();
            Ok(())
        })??;
        wait(cx, started, |cx| ready(case, &panel.borrow(), owner, cx)).await?;
        evidence
    } else {
        Value::Null
    };
    mark(report, case, "click_real_production_button_once");
    let (input, point) = handle.update(cx, |_, window, _| {
        button_input(&panel.borrow(), window, case.action.button())
    })??;
    input.click(point); // 原生目标HWND后台鼠标消息；不使用Space或直接调用action。
    mark(report, case, "wait_production_transition_and_reader_exit");
    wait(cx, started, |cx| {
        if matches!(case.action, Action::Done) {
            return Ok(panel_gone(&panel.borrow()) && previews_gone(owner));
        }
        let idle = handle.update(cx, |view, _, _| -> Result<bool> {
            unchanged(view, &panel.borrow())?;
            Ok(view.stage == Stage::Idle
                && view.result.is_none()
                && view.output.is_none()
                && view.preview.is_none()
                && view
                    .session_files
                    .writeback_request(&case.original)
                    .is_err())
        })??;
        Ok(idle && previews_gone(owner))
    })
    .await?;
    mark(report, case, "check_strict_files_without_cleanup_retry");
    let files = case.file_gate(fixture, &replacements)?;
    report.borrow_mut()["cases"][case.name]["files"] = files;
    report.borrow_mut()["cases"][case.name]["ok"] = true.into();
    report.borrow_mut()["cases"][case.name]["source_rule"] = match case.action {
        Action::Rerecord => {
            "Idle; result/output/preview cleared; old original writeback qualification revoked"
        }
        Action::Done => {
            "Production Done removed its panel and current preview; all tracked readers exited"
        }
    }
    .into();
    Ok(())
}

async fn cleanup(owner: &Owner, cx: &mut AsyncApp) -> Result<Value> {
    // 仅销毁窗口/停止后台，绝不补删或重试产品的媒体清理；body失败仍保持失败。
    for panel in owner.panels.borrow().clone() {
        if panel_gone(&panel.borrow()) {
            continue;
        }
        let handle = panel.borrow().handle;
        let result = handle.update(cx, |view, window, cx| {
            if view.preview.is_some() {
                let _ = track_preview(view, owner, cx);
            }
            view.close_preview(cx);
            view.border.take();
            view.tip.take();
            view.toast.take();
            window.remove_window();
        });
        if result.is_err() && !panel_gone(&panel.borrow()) {
            owner.tracking_failed.set(true);
        }
    }
    for preview in owner.previews.borrow().clone() {
        let _ = preview
            .handle
            .update(cx, |_, window, _| window.remove_window());
    }
    let started = Instant::now();
    while started.elapsed() < WAIT {
        if released(owner) {
            return Ok(json!({"explicit_teardown_released":true,
            "panel_count":owner.panels.borrow().len(),"preview_generations":owner.previews.borrow().len(),
            "media_cleanup_retried":false}));
        }
        tick(cx).await;
    }
    Err("显式兜底后仍有自有窗口/后台存活或探针未知".into())
}

fn diagnostic(owner: &Owner, cx: &mut AsyncApp) -> Value {
    let mut panels = Vec::new();
    for panel in owner.panels.borrow().clone() {
        let tracked = panel.borrow();
        let observed = tracked.handle.update(cx, |view, _, _| json!({
            "stage":format!("{:?}",view.stage),"result":view.result.as_ref().map(|result|&result.path),
            "output":view.output,"preview_present":view.preview.is_some(),
            "recording":view.recording.is_some(),"countdown":view.countdown.is_some(),
            "mix_job":view.mix_job.is_some(),"gif_job":view.gif_job.is_some(),
            "notice":view.notice_text().chars().take(2048).collect::<String>()}));
        panels.push(match observed { Ok(value) => value, Err(error) => json!({"read_error":error.to_string().chars().take(2048).collect::<String>(),"gone":panel_gone(&tracked)}) });
    }
    let mut previews = Vec::new();
    for preview in owner.previews.borrow().clone() {
        let observed = preview.handle.update(cx, |view, _, _| json!({
            "error":view.player.error().map(|error|error.chars().take(2048).collect::<String>()),
            "size":view.player.size(),"playing":view.player.playing(),
            "pending":view.player.seeking(),"submitted":view.player.submitted_frame_point().map(|(serial,pts)|json!({"serial":serial,"pts_ns":pts.as_nanos()}))}));
        previews.push(json!({"video_exited":preview.worker.video_exited(),
            "all_workers_exited":preview.worker.finished(),"entity_released":preview.entity.upgrade().is_none(),
            "hwnd_exists":unsafe{IsWindow(Some(HWND(preview.native as *mut _)))}.as_bool(),
            "player":match observed {Ok(value)=>value,Err(error)=>json!({"read_error":error.to_string().chars().take(2048).collect::<String>()})}}));
    }
    json!({"tracking_failed":owner.tracking_failed.get(),"panels":panels,"previews":previews})
}

fn inventory(fixture: &Fixture, cases: &[Case]) -> Value {
    let mut rows = Vec::new();
    for case in cases {
        for path in [&case.original, &case.auxiliary] {
            for candidate in [path.clone(), path.with_extension("registered-backup.mp4")] {
                let value = match file_snapshot::inspect(&candidate) {
                    Ok(Some(value)) => {
                        json!({"snapshot":snapshot_value(&value),"bounded_bytes_read":read_owned(&candidate).map(|bytes| bytes.len()).ok()})
                    }
                    Ok(None) => json!({"missing":true}),
                    Err(error) => {
                        json!({"error":error.to_string().chars().take(2048).collect::<String>()})
                    }
                };
                rows.push(json!({"path":candidate,"observed":value}));
            }
        }
    }
    json!({"root":fixture.root,"fixed_case_files":rows})
}

fn run() -> Result<()> {
    let started = Instant::now();
    let mut fixture = Fixture::new()?;
    let report = Rc::new(RefCell::new(
        json!({"ok":false,"mode":"production_done_rerecord_ownership",
        "cases":{},"scope":["One dedicated exact GPUI process; fresh RUSTSHOT_DATA_DIR before native/App/options initialization, system theme. No recording, desktop pixels, camera, microphone, audio, clipboard, save dialog or physical input.",
        "A fully finalized and EOF-decoded owned video-only H264 baseline is create_new-copied to each original and auxiliary. Production finish_recording registers both. Four fixed cases use two toolbars and six preview generations.",
        "Replacement setup closes and awaits the old reader, renames registered objects to retained backups, creates new identities at the same paths, and only reopens the preview without re-registering ownership.",
        "Owned deletion is mandatory. Failure is not repaired by cleanup retries. Done/rerecord success requires their real posted-mouse callback transition before explicit test teardown. Private SessionFiles collections are not directly inspected.",
        "Waits are cooperative between calls and cannot interrupt native/I/O calls; external isolation remains required. All files not deleted by the production action are retained."],
        "file_limit_bytes":FILE_LIMIT,"fixed_media_file_count_maximum":13,
        "bounded_case_copies_plus_baseline_bytes":FILE_LIMIT*13,
        "native_baseline_file_limit_is_post_call_observation_not_hard_disk_quota":true,
        "report_limit_bytes":REPORT_LIMIT,"ui_cooperative_budget_ms":UI_BUDGET.as_millis()}),
    ));
    let setup = std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<Vec<Case>> {
        report.borrow_mut()["baseline"] = fixture.produce()?;
        [
            ("owned_rerecord", Action::Rerecord, false),
            ("replaced_rerecord", Action::Rerecord, true),
            ("replaced_done", Action::Done, true),
            ("owned_done", Action::Done, false),
        ]
        .into_iter()
        .map(|(name, action, replaced)| Case::create(&fixture, name, action, replaced))
        .collect()
    }));
    let cases = match setup {
        Ok(Ok(cases)) => Rc::new(cases),
        other => {
            report.borrow_mut()["setup_error"] = match other {
                Ok(Err(error)) => error.to_string(),
                _ => "基准/案例准备捕获panic".into(),
            }
            .into();
            write_report(&fixture.root, &report.borrow())?;
            return Err("来源专项准备失败，产物已保留".into());
        }
    };
    let fixture = Rc::new(fixture);
    let owner = Rc::new(Owner::default());
    let outcome = Rc::new(RefCell::new(Some("GPUI专项未完成".to_owned())));
    let (app_fixture, app_cases, app_owner, app_report, app_outcome) = (
        fixture.clone(),
        cases.clone(),
        owner.clone(),
        report.clone(),
        outcome.clone(),
    );
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
                    let body = AssertUnwindSafe(async {
                        let started = Instant::now();
                        let panel = open_panel(&app_owner, cx)?;
                        for case in app_cases.iter().take(3) {
                            exercise_case(
                                &app_fixture,
                                case,
                                &panel,
                                &app_owner,
                                cx,
                                &app_report,
                                started,
                            )
                            .await?;
                        }
                        let panel = open_panel(&app_owner, cx)?;
                        exercise_case(
                            &app_fixture,
                            &app_cases[3],
                            &panel,
                            &app_owner,
                            cx,
                            &app_report,
                            started,
                        )
                        .await?;
                        ensure(
                            app_owner.panels.borrow().len() == 2
                                && app_owner.previews.borrow().len() == 6,
                            "实际工具栏或Player代数不符合固定案例",
                        )?;
                        Ok::<(), Box<dyn std::error::Error>>(())
                    })
                    .catch_unwind()
                    .await;
                    let body_error = match body {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("交互捕获panic".into()),
                    };
                    match std::panic::catch_unwind(AssertUnwindSafe(|| diagnostic(&app_owner, cx)))
                    {
                        Ok(value) => app_report.borrow_mut()["before_teardown"] = value,
                        Err(_) => {
                            app_owner.tracking_failed.set(true);
                            app_report.borrow_mut()["diagnostic_error"] =
                                "退出观测捕获panic".into();
                        }
                    }
                    let closed = AssertUnwindSafe(cleanup(&app_owner, cx))
                        .catch_unwind()
                        .await;
                    let cleanup_error = match closed {
                        Ok(Ok(value)) => {
                            app_report.borrow_mut()["teardown"] = value;
                            None
                        }
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("窗口兜底捕获panic".into()),
                    };
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
        report.borrow_mut()["application_error"] = "Application捕获panic".into();
    }
    let host_gone = owner
        .host_entity
        .borrow()
        .as_ref()
        .is_some_and(|entity| entity.upgrade().is_none())
        && owner.host_native.get() != 0
        && !unsafe { IsWindow(Some(HWND(owner.host_native.get() as *mut _))) }.as_bool();
    let resources = released(&owner) && host_gone;
    let all_cases = cases
        .iter()
        .all(|case| report.borrow()["cases"][case.name]["ok"] == true);
    // 结束后沿动作时实际记录的替换身份重读，防后继案例或 teardown 改变早先
    // 应保留的对象。缺证据/读错均失败，不能靠 inventory 或期望值补造身份。
    let mut final_case_files = true;
    for case in cases.iter() {
        let replacement = report.borrow()["cases"][case.name]["replacement"].clone();
        let observed = match case.file_gate(&fixture, &replacement) {
            Ok(files) => json!({"ok":true,"files":files}),
            Err(error) => {
                final_case_files = false;
                json!({"ok":false,"error":error.to_string().chars().take(2048).collect::<String>()})
            }
        };
        report.borrow_mut()["final_file_gates"][case.name] = observed;
    }
    let baseline_retained = (|| -> Result<bool> {
        let path = fixture.root.join("baseline.mp4");
        Ok(fixture.baseline.is_some()
            && file_snapshot::inspect(&path)?.as_ref() == fixture.baseline.as_ref()
            && read_owned(&path)? == fixture.bytes)
    })();
    let baseline_ok = matches!(&baseline_retained, Ok(true));
    report.borrow_mut()["baseline_retained"] = match baseline_retained {
        Ok(value) => json!({"snapshot_and_all_bytes_preserved":value}),
        Err(error) => json!({"error":error.to_string().chars().take(2048).collect::<String>()}),
    };
    let ok = application.is_ok()
        && outcome.borrow().is_none()
        && resources
        && all_cases
        && final_case_files
        && baseline_ok;
    report.borrow_mut()["resources_released_after_application"] = resources.into();
    report.borrow_mut()["remaining_artifacts"] = inventory(&fixture, &cases);
    if let Some(error) = outcome.borrow().as_ref() {
        report.borrow_mut()["run_error"] = error.chars().take(2048).collect::<String>().into();
    }
    report.borrow_mut()["ok"] = ok.into();
    report.borrow_mut()["elapsed_ms"] = json!(started.elapsed().as_millis());
    write_report(&fixture.root, &report.borrow())?;
    assert!(
        ok,
        "Done/重录来源专项失败；产物与报告：{}",
        fixture.root.display()
    );
    Ok(())
}

#[test]
#[ignore = "独立exact GPUI专项；只点Done/重录回Idle，自有无音轨素材，不采设备或桌面"]
fn native_recording_preview_done_rerecord_ownership_smoke() -> Result<()> {
    run()
}
