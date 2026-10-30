//! 独立 exact ignored 专项：自有未完成录制的真实发现页与 Verify 按钮。
//! 不安装主窗口单例，不打开用户历史、设备、保存对话框或文件播放器。
use super::super::super::{main_page::MainPage, main_pins, translation, SettingsTab};
use super::super::HistoryPage;
use super::*;
use crate::{
    file_snapshot::{self, FileSnapshot},
    gpui_capture::hwnd,
    gpui_smoke::Input,
    media_foundation::{MfRuntime, Mp4Writer, VideoReader},
    preferences::Preferences,
    Result,
};
use futures::FutureExt;
use gpui::{
    size, AppContext, Application, AsyncApp, Bounds, Render, WeakEntity, WindowBounds,
    WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    panic::AssertUnwindSafe,
    rc::Rc,
    time::Instant,
};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Media::MediaFoundation::MFVideoFormat_H264,
    Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE},
    UI::WindowsAndMessaging::{GetClientRect, IsWindow},
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
// 使用宿主实际 settings_probe 的 key，不按 ElementId 或布局推算坐标。
const ENTRY_PROBE: &str = "history-recovery-open";
const VERIFY_PROBE_PREFIX: &str = "history-recovery-verify-";

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
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

fn snapshot(path: &Path) -> Result<FileSnapshot> {
    file_snapshot::inspect(path)?.ok_or_else(|| "专项自有文件不存在".into())
}

fn snapshot_value(value: &FileSnapshot) -> Value {
    json!({"volume":value.volume,"id":value.id,"created":value.created,
        "written":value.written,"bytes":value.bytes,"attributes":value.attributes})
}

fn exclusive_lease(directory: &Path) -> Result<Value> {
    // 固定会话元数据名；只操作本专项 create_dir 创建的 staging 目录。
    let path = directory.join("recovery.lease");
    let metadata = fs::symlink_metadata(&path)?;
    ensure(
        metadata.is_file() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
        "专项租约不是安全普通文件",
    )?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    lease
        .try_lock()
        .map_err(|error| format!("专项未取得 OS 排他租约：{error}"))?;
    drop(lease);
    Ok(json!({"exclusive_lock_acquired":true,"released_by_drop":true}))
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    library: PathBuf,
    staging: PathBuf,
    source: PathBuf,
    previous_data: Option<OsString>,
    previous_experimental: Option<OsString>,
    bytes: Vec<u8>,
    baseline: Option<FileSnapshot>,
    source_before: Option<FileSnapshot>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::path::absolute("target/recording-recovery-ui")?;
        fs::create_dir_all(&parent)?;
        let metadata = fs::symlink_metadata(&parent)?;
        ensure(
            metadata.is_dir() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
            "恢复界面专项根目录不可用",
        )?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        for index in 0..16 {
            let root = parent.join(format!("native-{}-{stamp}-{index}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let data = root.join("data");
                    let library = data.join("recordings");
                    let staging = library.join(".rustshot-recording-ui-owned");
                    let fixture = Self {
                        source: staging.join(MediaName::Video.basename()),
                        root,
                        data,
                        library,
                        staging,
                        previous_data: std::env::var_os("RUSTSHOT_DATA_DIR"),
                        previous_experimental: std::env::var_os("RUSTSHOT_EXPERIMENTAL_MEDIA"),
                        bytes: Vec::new(),
                        baseline: None,
                        source_before: None,
                    };
                    // 早于 Preferences、theme、MF、Application 及任何原生窗口初始化。
                    std::env::set_var("RUSTSHOT_DATA_DIR", &fixture.data);
                    std::env::set_var("RUSTSHOT_EXPERIMENTAL_MEDIA", "1");
                    return Ok(fixture);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("恢复界面专项独占目录冲突过多".into())
    }

    fn produce(&mut self) -> Result<Value> {
        fs::create_dir(&self.data)?;
        fs::create_dir(&self.library)?;
        fs::create_dir(&self.staging)?;
        ensure(
            std::path::absolute(crate::recording::library_dir())? == self.library,
            "专项未隔离真实录屏库",
        )?;
        let started = Instant::now();
        let _runtime = MfRuntime::new()?;
        let baseline = self.root.join("baseline.mp4");
        ensure(!baseline.try_exists()?, "基准路径已经存在")?;
        let mut writer = Mp4Writer::new(&baseline, WIDTH, HEIGHT, FPS, 4_000_000, false)?;
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
        self.bytes = read_owned(&baseline)?;
        ensure(!self.bytes.is_empty(), "完整基准为空")?;
        self.baseline = Some(snapshot(&baseline)?);
        let mut reader = VideoReader::open(&baseline)?;
        ensure(
            reader.info.codec == MFVideoFormat_H264
                && reader.width == WIDTH
                && reader.height == HEIGHT,
            "基准类型或尺寸不符",
        )?;
        let stop = AtomicBool::new(false);
        let mut frames = 0;
        while let Some(frame) = reader.next_cancellable(&stop)? {
            ensure(
                frames < FRAMES && started.elapsed() < Duration::from_secs(15),
                "基准完整回读超限",
            )?;
            ensure(
                frame.bgra.len() == pixels.len()
                    && frame.time.abs_diff(frame_time(frames)) <= Duration::from_millis(1),
                "基准完整回读布局或 PTS 不符",
            )?;
            frames += 1;
        }
        ensure(
            frames == FRAMES && started.elapsed() < Duration::from_secs(15),
            "基准未在调用间预算内完整回读至 EOF",
        )?;
        drop(reader);
        write_new(&self.source, &self.bytes)?;
        let source_before = snapshot(&self.source)?;
        let mut session = model::Session::create(&self.staging)?;
        session.register_file(MediaName::Video, &source_before)?;
        session.set_phase(Phase::Incomplete)?;
        self.source_before = Some(source_before);
        drop(session); // UI 扫描前真实释放创建会话持有的 OS 锁。
        let lease = exclusive_lease(&self.staging)?;
        Ok(
            json!({"writer":diagnostics,"video_frames":frames,"video_eof":true,
            "audio_requested":false,"bytes":self.bytes.len(),"pts_tolerance_ns":1_000_000,
            "session_phase":"incomplete","lease_after_session_drop":lease,
            "source":self.preserved()?}),
        )
    }

    fn preserved(&self) -> Result<Value> {
        let before = self.source_before.as_ref().ok_or("专项来源身份未登记")?;
        ensure(
            snapshot(&self.source)? == *before && read_owned(&self.source)? == self.bytes,
            "恢复页动作改变或删除了登记原片",
        )?;
        let baseline = self.root.join("baseline.mp4");
        ensure(
            self.baseline.is_some()
                && file_snapshot::inspect(&baseline)?.as_ref() == self.baseline.as_ref()
                && read_owned(&baseline)? == self.bytes,
            "恢复页动作改变或删除了健康基准",
        )?;
        Ok(json!({"source_snapshot":snapshot_value(before),
            "source_snapshot_and_all_bytes_preserved":true,
            "baseline_snapshot_and_all_bytes_preserved":true}))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, previous) in [
            ("RUSTSHOT_DATA_DIR", &self.previous_data),
            ("RUSTSHOT_EXPERIMENTAL_MEDIA", &self.previous_experimental),
        ] {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        // 原片、基准、恢复元数据及失败产物全部保留。
    }
}

fn frame_time(index: u32) -> Duration {
    Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(FPS))
}

fn memory_settings(window: &mut Window, cx: &mut Context<SettingsView>) -> SettingsView {
    let preferences = Preferences::default();
    let positions = Rc::new(RefCell::new(HashMap::new()));
    let save_settings = cx
        .new(|cx| crate::settings_save::SaveSettings::new_in_memory(window, positions.clone(), cx));
    let mut history = HistoryPage::new(window, cx);
    // 保持真实历史模型为空；不调用 activate/open_unique 或正常历史扫描。
    history.scanned_at = Some(Instant::now());
    SettingsView {
        quick_actions: crate::settings_quick_actions::QuickActionSettings::new_in_memory(
            window, cx,
        ),
        tab: SettingsTab::default(),
        toolbar_config: crate::toolbar_config::ToolbarConfig::default(),
        autostart: Err("memory-only recovery UI probe".into()),
        status: String::new(),
        hotkey_focus: cx.focus_handle(),
        recording_hotkey: false,
        recording_hotkey_target: crate::hotkey::HotkeyTarget::Capture,
        hotkey_request: None,
        hotkey_request_started: 0,
        hotkey_error: None,
        recording_native: None,
        hotkey_native: None,
        trigger_recorder: None,
        recording_held: Vec::new(),
        recording_generation: 0,
        _hotkey_subscriptions: Vec::new(),
        ui_positions: positions,
        save_settings,
        translation_fields: translation::TranslationFields::new(&preferences, window, cx),
        preferences,
        page: MainPage::History,
        pins: main_pins::PinsPage::new(window, cx),
        history,
        media_popup: super::super::super::main_media_popup::MediaPopup::default(),
        tool_popup: super::super::super::main_tool_popup::ToolPopup::default(),
        _instance: None,
        toast: Default::default(),
    }
}

struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().bg(rgb(crate::theme::palette(cx).background))
    }
}

#[derive(Default)]
struct Owner {
    window: Cell<Option<WindowHandle<SettingsView>>>,
    entity: RefCell<Option<WeakEntity<SettingsView>>>,
    native: Cell<usize>,
    host: Cell<Option<WindowHandle<Host>>>,
    host_entity: RefCell<Option<WeakEntity<Host>>>,
    host_native: Cell<usize>,
    scans: RefCell<Vec<Arc<AtomicBool>>>,
    works: RefCell<Vec<Arc<RecoveryWork>>>,
    entry_clicked: Cell<bool>,
    verify_clicked: Cell<bool>,
    scan_receipt_seen: Cell<bool>,
    verify_receipt_seen: Cell<bool>,
    tracking_failed: Cell<bool>,
}

fn track_jobs(view: &SettingsView, owner: &Owner) -> Result<()> {
    let result = (|| -> Result<()> {
        let panel = &view.history.recovery;
        if let Some(cancel) = &panel.running {
            let known = owner
                .scans
                .borrow()
                .iter()
                .any(|old| Arc::ptr_eq(old, cancel));
            if !known {
                ensure(owner.scans.borrow().len() < 2, "恢复扫描跟踪超过有界上限")?;
                owner.scans.borrow_mut().push(cancel.clone());
                ensure(owner.scans.borrow().len() == 1, "意外开始第二次恢复扫描")?;
            }
        }
        if let Some(work) = &panel.work {
            let known = owner
                .works
                .borrow()
                .iter()
                .any(|old| Arc::ptr_eq(old, work));
            if !known {
                ensure(owner.works.borrow().len() < 2, "恢复操作跟踪超过有界上限")?;
                owner.works.borrow_mut().push(work.clone());
                ensure(owner.works.borrow().len() == 1, "意外开始第二次恢复操作")?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        owner.tracking_failed.set(true);
    }
    result
}

fn jobs_released(owner: &Owner) -> bool {
    !owner.tracking_failed.get()
        && (!owner.entry_clicked.get()
            || owner.scan_receipt_seen.get()
            || !owner.scans.borrow().is_empty())
        && (!owner.verify_clicked.get()
            || owner.verify_receipt_seen.get()
            || !owner.works.borrow().is_empty())
        && owner
            .scans
            .borrow()
            .iter()
            .all(|arc| Arc::strong_count(arc) == 1)
        && owner
            .works
            .borrow()
            .iter()
            .all(|arc| Arc::strong_count(arc) == 1)
}

fn native_gone<T: 'static>(entity: &RefCell<Option<WeakEntity<T>>>, native: usize) -> bool {
    entity
        .borrow()
        .as_ref()
        .is_some_and(|entity| entity.upgrade().is_none())
        && native != 0
        && !unsafe { IsWindow(Some(HWND(native as *mut _))) }.as_bool()
}

fn invariant(view: &SettingsView, fixture: &Fixture) -> Result<()> {
    ensure(
        view.page == MainPage::History
            && view._instance.is_none()
            && view.history.items.is_empty()
            && view.history.pending.is_empty()
            && view.history.selected.is_none()
            && !view.history.loading
            && !view.recording_hotkey
            && view.recording_native.is_none()
            && view.hotkey_native.is_none()
            && view.trigger_recorder.is_none(),
        "专项意外进入用户历史、单例或输入录制路径",
    )?;
    ensure(
        crate::file_launch_command::experimental_media_enabled()
            && std::path::absolute(crate::recording::library_dir())? == fixture.library,
        "专项实验门或录屏库路径变化",
    )
}

fn button_input(
    view: &SettingsView,
    window: &Window,
    owner: &Owner,
    key: &str,
) -> Result<(Input, (f32, f32))> {
    let handle = hwnd(window);
    ensure(
        handle.0 as usize == owner.native.get() && unsafe { IsWindow(Some(handle)) }.as_bool(),
        "按钮不属于专项自有 HWND",
    )?;
    let point = view
        .ui_positions
        .borrow()
        .get(key)
        .copied()
        .ok_or("专项按钮尚未真实布局")?;
    let scale = window.scale_factor();
    let mut client = RECT::default();
    unsafe { GetClientRect(handle, &mut client) }?;
    ensure(
        scale.is_finite()
            && scale > 0.
            && point.0.is_finite()
            && point.1.is_finite()
            && point.0 > 0.
            && point.1 > 0.
            && point.0 * scale < client.right as f32
            && point.1 * scale < client.bottom as f32,
        "专项按钮不在自有窗口客户区",
    )?;
    Ok((
        Input {
            handle: owner.native.get(),
            scale,
        },
        point,
    ))
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor().timer(TICK).await;
}

async fn wait(
    cx: &mut AsyncApp,
    started: Instant,
    mut condition: impl FnMut(&mut AsyncApp) -> Result<bool>,
) -> Result<()> {
    let began = Instant::now();
    while began.elapsed() < WAIT && started.elapsed() < UI_BUDGET {
        if condition(cx)? {
            return Ok(());
        }
        tick(cx).await;
    }
    Err("恢复界面专项条件未在调用间预算内满足".into())
}

fn verification_gate(report: &VerificationReport, fixture: &Fixture) -> Result<()> {
    let source = fixture.source_before.as_ref().ok_or("专项原片缺少身份")?;
    ensure(
        report.directory == fixture.staging
            && report.name == MediaName::Video
            && report.state == VerificationState::VerifiedReadable
            && report.stage == verification::VerificationStage::Complete
            && report.error.is_none()
            && report.registered_identity_matches
            && report.metadata_stable
            && report.registration_content_unchanged == Some(true)
            && report.identity.as_ref().is_some_and(|identity| {
                identity.volume == source.volume
                    && identity.id == source.id
                    && identity.created == source.created
            })
            && report.bytes == Some(source.bytes)
            && report.registered_bytes == Some(source.bytes)
            && report.width == Some(WIDTH)
            && report.height == Some(HEIGHT)
            && report.video_frames == u64::from(FRAMES)
            && report.video_eof
            && report.audio_presence == verification::AudioPresence::Absent
            && !report.lease_held,
        "实际 Verify 回执没有完整验证本专项无音轨原片",
    )
}

async fn exercise(
    fixture: &Fixture,
    owner: &Owner,
    report: &Rc<RefCell<Value>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let started = Instant::now();
    report.borrow_mut()["stage"] = "open_memory_history".into();
    let handle = cx.update(|cx| -> Result<_> {
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                owner.native.set(hwnd(window).0 as usize);
                window.set_window_title(&format!(
                    "{} · {}",
                    crate::brand::NAME,
                    tr!("恢复发现检查", "Recovery discovery check")
                ));
                cx.new(|cx| {
                    *owner.entity.borrow_mut() = Some(cx.entity().downgrade());
                    memory_settings(window, cx)
                })
            },
        )?;
        owner.window.set(Some(handle));
        handle.update(cx, |view, window, cx| -> Result<()> {
            track_jobs(view, owner)?;
            crate::gpui_smoke::isolate_input(hwnd(window))?;
            invariant(view, fixture)?;
            ensure(!view.history.recovery.visible, "初始恢复页意外已打开")?;
            view.ui_positions.borrow_mut().clear();
            cx.notify();
            Ok(())
        })??;
        Ok(handle)
    })??;
    wait(cx, started, |cx| {
        handle.update(cx, |view, window, _| -> Result<bool> {
            track_jobs(view, owner)?;
            invariant(view, fixture)?;
            Ok(button_input(view, window, owner, ENTRY_PROBE).is_ok())
        })?
    })
    .await?;
    report.borrow_mut()["before_discovery_click"] = fixture.preserved()?;
    let (input, point) = handle.update(cx, |view, window, _| -> Result<_> {
        track_jobs(view, owner)?;
        invariant(view, fixture)?;
        button_input(view, window, owner, ENTRY_PROBE)
    })??;
    owner.entry_clicked.set(true);
    report.borrow_mut()["stage"] = "click_history_recovery_entry".into();
    input.click(point);
    let candidate_index = Cell::new(None);
    wait(cx, started, |cx| {
        handle.update(cx, |view, _, cx| -> Result<bool> {
            track_jobs(view, owner)?; // 先保留取消/任务所有权，再执行任何可失败资格门。
            invariant(view, fixture)?;
            let panel = &view.history.recovery;
            if !panel.visible || panel.running.is_some() {
                return Ok(false);
            }
            let Some((root, scan)) = &panel.report else {
                ensure(panel.message.is_none(), "实际发现扫描返回了错误提示")?;
                return Ok(false);
            };
            ensure(
                root == &fixture.library
                    && !scan.cancelled
                    && !scan.truncated
                    && !scan.lease_held
                    && scan.active_skipped == 0
                    && scan.sessions.len() == 1
                    && scan.checked_candidates == 1
                    && scan.diagnostics.is_empty()
                    && scan.omitted_diagnostics == 0
                    && panel.work.is_none()
                    && panel.verification.is_none(),
                "实际发现扫描没有只列出本专项会话",
            )?;
            let session = &scan.sessions[0];
            ensure(
                session.directory == fixture.staging
                    && session.phase == Phase::Incomplete
                    && !session.lease_held,
                "实际发现扫描会话来源或阶段不符",
            )?;
            let index = session
                .media
                .iter()
                .position(|media| media.name == MediaName::Video)
                .ok_or("实际扫描缺少 Video 条目")?;
            let media = &session.media[index];
            ensure(
                media.state == MediaState::Present
                    && media.identity_matches == Some(true)
                    && media.content_unchanged == Some(true)
                    && media.bytes == Some(fixture.bytes.len() as u64),
                "实际扫描的 Video 不是完整登记自有原片",
            )?;
            // 快速任务可在第一次 poll 前完成；精确、已应用的 UI 回执证明 task.await 已返回。
            owner.scan_receipt_seen.set(true);
            candidate_index.set(Some(index)); // 唯一会话 session_index=0，沿生产索引规则。
            report.borrow_mut()["discovery"] = serde_json::to_value(scan)?;
            view.ui_positions.borrow_mut().clear();
            cx.notify();
            Ok(true)
        })?
    })
    .await?;
    report.borrow_mut()["after_discovery"] = fixture.preserved()?;
    report.borrow_mut()["lease_after_discovery"] = exclusive_lease(&fixture.staging)?;
    let key = format!(
        "{VERIFY_PROBE_PREFIX}{}",
        candidate_index.get().ok_or("没有实际候选索引")?
    );
    wait(cx, started, |cx| {
        handle.update(cx, |view, window, _| -> Result<bool> {
            track_jobs(view, owner)?;
            invariant(view, fixture)?;
            Ok(view.history.recovery.visible
                && view.history.recovery.running.is_none()
                && view.history.recovery.work.is_none()
                && button_input(view, window, owner, &key).is_ok())
        })?
    })
    .await?;
    let (input, point) = handle.update(cx, |view, window, _| -> Result<_> {
        track_jobs(view, owner)?;
        invariant(view, fixture)?;
        ensure(
            view.history.recovery.running.is_none()
                && view.history.recovery.work.is_none()
                && view.history.recovery.verification.is_none(),
            "真实 Verify 点击前已有后台操作或旧验证回执",
        )?;
        button_input(view, window, owner, &key)
    })??;
    owner.verify_clicked.set(true);
    report.borrow_mut()["stage"] = "click_actual_candidate_verify".into();
    report.borrow_mut()["verify_probe"] = key.into();
    input.click(point);
    wait(cx, started, |cx| {
        handle.update(cx, |view, _, _| -> Result<bool> {
            track_jobs(view, owner)?;
            invariant(view, fixture)?;
            let panel = &view.history.recovery;
            ensure(
                panel.visible && panel.running.is_none(),
                "验证期间页面隐藏或并发扫描",
            )?;
            if panel.work.is_some() {
                return Ok(false);
            }
            let Some((directory, name, verified)) = &panel.verification else {
                ensure(
                    panel.message.is_none(),
                    "实际 Verify 返回错误提示而没有回执",
                )?;
                return Ok(false);
            };
            ensure(
                directory == &fixture.staging && *name == MediaName::Video,
                "显示了其它候选的验证回执",
            )?;
            owner.verify_receipt_seen.set(true);
            verification_gate(verified, fixture)?;
            report.borrow_mut()["verification"] = serde_json::to_value(verified)?;
            report.borrow_mut()["visible_result_text"] = verification_text(verified).into();
            Ok(true)
        })?
    })
    .await?;
    report.borrow_mut()["after_verify"] = fixture.preserved()?;
    report.borrow_mut()["lease_after_verify"] = exclusive_lease(&fixture.staging)?;
    report.borrow_mut()["stage"] = "discovery_and_verify_completed".into();
    Ok(())
}

fn jobs_value(owner: &Owner) -> Value {
    json!({"tracking_failed":owner.tracking_failed.get(),
        "entry_clicked":owner.entry_clicked.get(),"verify_clicked":owner.verify_clicked.get(),
        "scan_ui_receipt_seen":owner.scan_receipt_seen.get(),"verify_ui_receipt_seen":owner.verify_receipt_seen.get(),
        "observed_scan_arcs":owner.scans.borrow().iter().map(|arc| json!({"strong_count":Arc::strong_count(arc),"cancel_requested":arc.load(Ordering::Acquire)})).collect::<Vec<_>>(),
        "observed_work_arcs":owner.works.borrow().iter().map(|arc| json!({"strong_count":Arc::strong_count(arc),"cancel_requested":arc.cancel.load(Ordering::Acquire),"action":format!("{:?}",arc.action)})).collect::<Vec<_>>(),
        "all_observed_background_owners_released":jobs_released(owner)})
}

async fn cleanup(owner: &Owner, cx: &mut AsyncApp) -> Result<Value> {
    for cancel in owner.scans.borrow().iter() {
        cancel.store(true, Ordering::Release);
    }
    for work in owner.works.borrow().iter() {
        work.cancel.store(true, Ordering::Release);
    }
    if let Some(handle) = owner.window.get() {
        handle.update(cx, |view, _, cx| -> Result<()> {
            let tracked = track_jobs(view, owner);
            view.history.recovery.hide();
            cx.notify();
            tracked
        })??;
        // 保持实体/回执宿主存活，直到后台真的返回；hide/cancel 本身不算退出。
        let started = Instant::now();
        let mut returned = false;
        while started.elapsed() < WAIT {
            let slots_clear = handle.update(cx, |view, _, _| -> Result<bool> {
                track_jobs(view, owner)?;
                Ok(view.history.recovery.running.is_none() && view.history.recovery.work.is_none())
            })??;
            if slots_clear && jobs_released(owner) {
                returned = true;
                break;
            }
            tick(cx).await;
        }
        // 仅关闭自有窗口；后台未退出仍保留失败事实，不靠移除实体伪造完成。
        let removed = handle.update(cx, |_, window, _| window.remove_window());
        ensure(removed.is_ok(), "没有关闭专项自有主窗口")?;
        ensure(returned, "取消后后台未在清理预算内退出")?;
    }
    if let Some(host) = owner.host.get() {
        host.update(cx, |_, window, _| window.remove_window())?;
    }
    let started = Instant::now();
    while started.elapsed() < WAIT {
        if native_gone(&owner.entity, owner.native.get())
            && native_gone(&owner.host_entity, owner.host_native.get())
            && jobs_released(owner)
        {
            return Ok(
                json!({"main_window_and_entity_released":true,"host_released":true,
                "background":jobs_value(owner),"source_cleanup_attempted":false}),
            );
        }
        tick(cx).await;
    }
    Err("专项自有窗口、实体或后台释放未获证实".into())
}

fn write_report(root: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    ensure(bytes.len() <= REPORT_LIMIT, "恢复界面专项报告超过 256 KiB")?;
    write_new(&root.join("report.json"), &bytes)
}

fn run() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let report = Rc::new(RefCell::new(json!({"ok":false,
        "mode":"actual_recovery_discovery_and_verify_ui",
        "scope":["Dedicated exact process; fresh RUSTSHOT_DATA_DIR and experimental-media gate before Preferences/theme/MF/Application/native initialization, restored after Application exits. System theme and only owned windows/files.",
            "A finalized and fully EOF-decoded 12-frame video-only H264 baseline is create_new-copied, registered by a real recovery Session, marked Incomplete, and the Session is dropped before discovery.",
            "Actual production history entry and candidate Verify mouse callbacks use measured settings_probe positions on the owned HWND. The final typed UI report and its displayed text are observed; no pixel-presentation claim.",
            "No desktop pixels, camera, microphone, WASAPI, user history/media/configuration, clipboard, save dialog, singleton activation, foreground activation or Space input. Cancellation races, stale receipts and save UI are untested.",
            "Budgets are cooperative between calls and cannot interrupt blocking native/I/O work. Cancelling/hiding is not treated as worker exit; observed task Arcs and UI slots must be released. All media and metadata are retained."],
        "entry_probe":ENTRY_PROBE,"verify_probe_prefix":VERIFY_PROBE_PREFIX,
        "file_limit_bytes":FILE_LIMIT,"media_file_count":2,"report_limit_bytes":REPORT_LIMIT,
        "ui_cooperative_budget_ms":UI_BUDGET.as_millis(),
        "native_baseline_file_limit_is_post_call_observation_not_hard_disk_quota":true})));
    let setup = std::panic::catch_unwind(AssertUnwindSafe(|| fixture.produce()));
    match setup {
        Ok(Ok(value)) => report.borrow_mut()["fixture"] = value,
        other => {
            report.borrow_mut()["setup_error"] = match other {
                Ok(Err(error)) => error.to_string(),
                _ => "恢复页专项准备捕获 panic".into(),
            }
            .into();
            write_report(&fixture.root, &report.borrow())?;
            return Err("恢复页专项准备失败；产物已保留".into());
        }
    }
    let fixture = Rc::new(fixture);
    let owner = Rc::new(Owner::default());
    let outcome = Rc::new(RefCell::new(Some("恢复页专项未完成".to_owned())));
    let (app_fixture, app_owner, app_report, app_outcome) = (
        fixture.clone(),
        owner.clone(),
        report.clone(),
        outcome.clone(),
    );
    let application = std::panic::catch_unwind(AssertUnwindSafe(|| {
        Application::new()
            .with_assets(crate::toolbar_icons::IconAssets)
            .run(move |cx| {
                crate::theme::set_mode(crate::theme::ThemeMode::System, cx);
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
                    let body =
                        AssertUnwindSafe(exercise(&app_fixture, &app_owner, &app_report, cx))
                            .catch_unwind()
                            .await;
                    let body_error = match body {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("恢复页交互捕获 panic".into()),
                    };
                    match std::panic::catch_unwind(AssertUnwindSafe(|| jobs_value(&app_owner))) {
                        Ok(value) => app_report.borrow_mut()["before_teardown_background"] = value,
                        Err(_) => {
                            app_owner.tracking_failed.set(true);
                            app_report.borrow_mut()["diagnostic_error"] =
                                "退出观测捕获 panic".into();
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
                        Err(_) => Some("恢复页清理捕获 panic".into()),
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
                        if let Some(window) = app_owner.window.get() {
                            let _ = window.update(cx, |_, window, _| window.remove_window());
                        }
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
        report.borrow_mut()["application_error"] = "Application 捕获 panic".into();
    }
    let resources = native_gone(&owner.entity, owner.native.get())
        && native_gone(&owner.host_entity, owner.host_native.get())
        && jobs_released(&owner);
    let final_source = fixture.preserved();
    let source_ok = final_source.is_ok();
    report.borrow_mut()["final_source"] = match final_source {
        Ok(value) => value,
        Err(error) => json!({"error":error.to_string().chars().take(2048).collect::<String>()}),
    };
    let lease = exclusive_lease(&fixture.staging);
    let lease_ok = lease.is_ok();
    report.borrow_mut()["final_lease"] = match lease {
        Ok(value) => value,
        Err(error) => json!({"error":error.to_string().chars().take(2048).collect::<String>()}),
    };
    report.borrow_mut()["final_background"] = jobs_value(&owner);
    report.borrow_mut()["resources_released_after_application"] = resources.into();
    if let Some(error) = outcome.borrow().as_ref() {
        report.borrow_mut()["run_error"] = error.chars().take(2048).collect::<String>().into();
    }
    let ok = application.is_ok()
        && outcome.borrow().is_none()
        && resources
        && source_ok
        && lease_ok
        && owner.entry_clicked.get()
        && owner.verify_clicked.get()
        && owner.scan_receipt_seen.get()
        && owner.verify_receipt_seen.get();
    report.borrow_mut()["ok"] = ok.into();
    write_report(&fixture.root, &report.borrow())?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "恢复页专项报告：{}",
        fixture.root.join("report.json").display()
    );
    assert!(
        ok,
        "恢复页发现/验证专项失败；报告：{}",
        fixture.root.display()
    );
    Ok(())
}

#[test]
#[ignore = "独立 exact GPUI 恢复发现/Verify 专项；自有已收尾无音轨素材，不采桌面或设备"]
fn native_history_recovery_discovery_verify_smoke() -> Result<()> {
    run()
}
