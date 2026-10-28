//! 实际 RecordingPreview 窗口专项；全零声音夹具，不覆盖录制宿主工具栏或屏幕显现。
use super::*;
use crate::{
    gpui_smoke::Input,
    media_foundation::{AudioReader, Mp4Writer},
    preview_audio_clock::ClockPhase,
    preview_audio_device_clock::PublishedClock,
    wav_track::WavWriter,
};
use gpui::{AppContext, Application, AsyncApp, WeakEntity};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    path::Path,
    sync::Weak,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY},
    Media::MediaFoundation::{MFAudioFormat_AAC, MFMediaType_Audio, MFVideoFormat_H264},
};

const WAIT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(25);

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

struct Fixture {
    root: PathBuf,
    previous_data: Option<OsString>,
    times: Vec<Duration>,
}
impl Fixture {
    fn new() -> Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustshot-recording-preview-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root)?;
        let mut fixture = Self {
            root,
            previous_data: std::env::var_os("RUSTSHOT_DATA_DIR"),
            times: Vec::new(),
        };
        std::env::set_var("RUSTSHOT_DATA_DIR", &fixture.root);
        fixture.write()?;
        Ok(fixture)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    fn tracks(&self) -> Vec<(AudioSource, PathBuf)> {
        vec![
            (AudioSource::System, self.path("system.wav")),
            (AudioSource::Microphone, self.path("microphone.wav")),
        ]
    }
    fn write(&mut self) -> Result<()> {
        let _runtime = MfRuntime::new()?;
        let path = self.path("preview.mp4");
        let mut writer = Mp4Writer::new(&path, 320, 240, 10, 600_000, true)?;
        let silence = vec![0_i16; 9_600];
        let raw_zero = vec![0_f32; 9_600];
        let mut system = WavWriter::create(&self.path("system.wav"))?;
        let mut microphone = WavWriter::create(&self.path("microphone.wav"))?;
        let mut pixels = vec![0_u8; 320 * 240 * 4];
        for index in 0..30_u64 {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[16, 24 + index as u8 * 2, 32, 255]);
            }
            let pts = Duration::from_millis(index * 100);
            writer.write_video(&pixels, 320, pts, Duration::from_millis(100))?;
            writer.write_audio(&silence, pts)?;
            system.write(&raw_zero)?;
            microphone.write(&raw_zero)?;
        }
        writer.finish()?;
        system.finish()?;
        microphone.finish()?;
        let mut reader = VideoReader::open(&path)?;
        ensure(
            reader.info.codec == MFVideoFormat_H264
                && reader.info.stream_probe_error.is_none()
                && reader.info.streams.iter().any(|stream| {
                    stream.major_type == MFMediaType_Audio
                        && stream.codec == Some(MFAudioFormat_AAC)
                }),
            "录屏窗口夹具缺少真实H264/AAC",
        )?;
        let deadline = Instant::now() + WAIT;
        for _ in 0..32 {
            ensure(Instant::now() < deadline, "录屏窗口夹具视频回读超时")?;
            let Some(frame) = reader.next()? else {
                break;
            };
            self.times.push(frame.time);
        }
        ensure(
            self.times.len() == 30 && self.times.windows(2).all(|pair| pair[0] < pair[1]),
            "录屏夹具PTS不完整或无序",
        )?;
        for (_, path) in self.tracks() {
            verify_zero_audio(&path, true)?;
        }
        verify_zero_audio(&path, false)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.previous_data {
            Some(path) => std::env::set_var("RUSTSHOT_DATA_DIR", path),
            None => std::env::remove_var("RUSTSHOT_DATA_DIR"),
        }
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.root);
    }
}

fn verify_zero_audio(path: &Path, wav: bool) -> Result<()> {
    // 与录屏分轨预览同一个MF读取入口；不将保存用WavBlockReader说成播放后端。
    let mut reader = AudioReader::open_for_preview(path)?.ok_or("全零夹具没有声音轨道")?;
    ensure(
        (reader.rate, reader.channels) == (48_000, 2),
        "全零夹具不是48k双声道",
    )?;
    let mut frames = 0;
    let deadline = Instant::now() + WAIT;
    for _ in 0..256 {
        ensure(Instant::now() < deadline, "全零声音夹具回读超时")?;
        let Some(block) = reader.next()? else {
            ensure(
                if wav { frames == 144_000 } else { frames > 0 },
                "全零声音夹具帧数错误",
            )?;
            return Ok(());
        };
        ensure(
            block.samples.len() <= 192_000 && block.samples.len() % 2 == 0,
            "全零声音夹具块超过有界格式",
        )?;
        ensure(
            block.samples.iter().all(|sample| {
                sample.is_finite()
                    && if wav {
                        *sample == 0.
                    } else {
                        sample.abs() < 0.000_001
                    }
            }),
            "声音夹具不是静音",
        )?;
        frames += block.samples.len() / 2;
    }
    Err("全零声音夹具没有有限结束".into())
}

struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
#[derive(Default)]
struct Owner {
    window: Cell<Option<WindowHandle<RecordingPreview>>>,
    entity: RefCell<Option<WeakEntity<RecordingPreview>>>,
    native: Cell<usize>,
    workers: RefCell<Option<PlaybackShutdownProbe>>,
    images: RefCell<Vec<Weak<RenderImage>>>,
}
struct Observed {
    serial: u64,
    pts: Duration,
    position: Duration,
    playing: bool,
    pending: bool,
    pending_frame: bool,
    generation: u64,
    clock: PublishedClock,
}

fn observe(
    window: WindowHandle<RecordingPreview>,
    owner: &Owner,
    cx: &mut AsyncApp,
) -> Result<Option<Observed>> {
    window.update(cx, |view, native, _| -> Result<_> {
        let player = &view.player;
        if let Some(error) = player.error().or_else(|| player.audio_error()) {
            return Err(error.into());
        }
        ensure(
            hwnd(native).0 as usize == owner.native.get()
                && unsafe { IsWindow(Some(hwnd(native))) }.as_bool(),
            "录屏预览HWND身份变化",
        )?;
        ensure(
            player.audio_sources() == [AudioSource::System, AudioSource::Microphone]
                && player.audio_selection().is_none(),
            "录屏窗口误走文件单轨分支",
        )?;
        match player.audio_info() {
            Some(formats) => ensure(formats == [(48_000, 2), (48_000, 2)], "录屏两路WAV格式错误")?,
            None => return Ok(None),
        }
        let Some((serial, image)) = &view.image else {
            return Ok(None);
        };
        let Some((submitted, pts)) = player.submitted_frame_point() else {
            return Ok(None);
        };
        if *serial != submitted {
            return Ok(None);
        }
        let mut images = owner.images.borrow_mut();
        if !images
            .iter()
            .any(|previous| previous.ptr_eq(&Arc::downgrade(image)))
        {
            ensure(images.len() < 64, "录屏窗口专项图像身份超过有界帧数")?;
            images.push(Arc::downgrade(image));
        }
        drop(images);
        let status = player.status.lock().unwrap();
        let (playing, pending, pending_frame, position, generation) = (
            status.playing,
            status.pending_seek.is_some() || status.pending_step,
            status.frame.is_some(),
            status.position,
            status.generation,
        );
        drop(status);
        let Some(clock) = qpc_now_100ns().and_then(|qpc| player.audio_clock_at(qpc)) else {
            return Ok(None);
        };
        ensure(
            player.sync_report()["groups"]
                .as_array()
                .is_some_and(|groups| {
                    groups
                        .iter()
                        .all(|group| group["stage"] != "presented" || group["accepted_total"] == 0)
                }),
            "录屏窗口不能冒充Presented证据",
        )?;
        Ok(Some(Observed {
            serial: *serial,
            pts,
            position,
            playing,
            pending,
            pending_frame,
            generation,
            clock,
        }))
    })?
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor().timer(TICK).await;
}
async fn wait(
    window: WindowHandle<RecordingPreview>,
    owner: &Owner,
    cx: &mut AsyncApp,
    ready: impl Fn(&Observed) -> bool,
) -> Result<Observed> {
    let deadline = Instant::now() + WAIT;
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(observed) = observe(window, owner, cx)? {
            if ready(&observed) {
                return Ok(observed);
            }
            last = Some(point(&observed));
        }
        tick(cx).await;
    }
    Err(format!("录屏预览窗口控制/实际提交/设备钟未在时限内就绪：last={last:?}").into())
}

async fn paused(
    window: WindowHandle<RecordingPreview>,
    owner: &Owner,
    cx: &mut AsyncApp,
    generation: u64,
    label: &str,
) -> Result<Observed> {
    // Pause 的UI意图先发布，设备结算与视频worker确认随后完成；单槽中此前已发布的
    // 帧还可能等待pull/paint。先等同代设备冻结、视频位置一致及真实提交消费完毕，再取基准。
    let first = wait(window, owner, cx, |observed| {
        observed.generation == generation
            && observed.clock.transport_generation == generation
            && !observed.playing
            && !observed.pending
            && !observed.pending_frame
            && observed.clock.clock.phase == ClockPhase::Paused
            && !observed.clock.clock.advancing
            && observed.position == observed.clock.clock.media_position
    })
    .await
    .map_err(|error| format!("{label} 暂停基准尚未完成结算：{error}"))?;
    let since = Instant::now();
    while since.elapsed() < Duration::from_millis(225) {
        tick(cx).await;
        if let Some(next) = observe(window, owner, cx)? {
            let stable = (
                next.generation,
                next.serial,
                next.pts,
                next.position,
                next.clock.clock.media_position,
            ) == (
                first.generation,
                first.serial,
                first.pts,
                first.position,
                first.clock.clock.media_position,
            ) && !next.playing
                && !next.pending
                && !next.pending_frame
                && next.clock.transport_generation == generation
                && next.clock.clock.generation == first.clock.clock.generation
                && next.clock.clock.phase == ClockPhase::Paused
                && !next.clock.clock.advancing;
            if !stable {
                return Err(format!("{label} 录屏窗口暂停期间帧/位置/设备钟推进：elapsed_ms={:.3}, baseline={}, current={}",
                    since.elapsed().as_secs_f64() * 1000., point(&first), point(&next)).into());
            }
        } else {
            return Err(format!(
                "{label} 录屏窗口暂停期间丢失实际提交/设备钟观测：baseline={}",
                point(&first)
            )
            .into());
        }
    }
    Ok(first)
}

fn region() -> Result<ScreenRect> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(
            MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY),
            &mut info,
        )
        .ok()?;
    }
    ensure(
        info.rcWork.right - info.rcWork.left >= 368 && info.rcWork.bottom - info.rcWork.top >= 288,
        "主显示器工作区不足以放置自有320×240结果窗",
    )?;
    Ok(ScreenRect {
        left: info.rcWork.left + 24,
        top: info.rcWork.top + 24,
        width: 320,
        height: 240,
    })
}

fn point(observed: &Observed) -> Value {
    json!({"serial":observed.serial,"pts_ms":observed.pts.as_secs_f64() * 1000.,
        "position_ns":observed.position.as_nanos(),"position_ms":observed.position.as_secs_f64() * 1000.,
        "playing":observed.playing,"pending":observed.pending,"pending_frame":observed.pending_frame,
        "transport_generation":observed.generation,"clock_transport_generation":observed.clock.transport_generation,
        "device_epoch":observed.clock.clock.generation,
        "device_media_ns":observed.clock.clock.media_position.as_nanos(),
        "device_media_ms":observed.clock.clock.media_position.as_secs_f64() * 1000.,
        "device_phase":format!("{:?}",observed.clock.clock.phase),"device_advancing":observed.clock.clock.advancing,
        "device_queued_media_ns":observed.clock.clock.queued_media.as_nanos(),
        "device_qpc_100ns":observed.clock.clock.qpc_100ns})
}

async fn exercise(fixture: &Fixture, owner: &Owner, cx: &mut AsyncApp) -> Result<Value> {
    let moves = Rc::new(Cell::new(0_u32));
    let moved = moves.clone();
    let (window, input) = cx.update(|cx| -> Result<_> {
        let window = RecordingPreview::open(
            region()?,
            fixture.path("preview.mp4"),
            fixture.tracks(),
            MixLevels::default(),
            Rc::new(move |_, _| moved.set(moved.get() + 1)),
            cx,
        )?;
        owner.window.set(Some(window));
        let input = window.update(cx, |view, native, cx| -> Result<_> {
            // 输入在构造前已回读为全零；以下设置额外保护，不掩盖构造时声音泄漏。
            view.player.set_volume(0.);
            view.player.set_muted(true);
            view.player.set_playing(false);
            view.player.seek_ratio(0.);
            let workers = view.player.shutdown_probe();
            *owner.entity.borrow_mut() = Some(cx.entity().downgrade());
            owner.native.set(hwnd(native).0 as usize);
            let has_audio = workers.audio.is_some();
            *owner.workers.borrow_mut() = Some(workers);
            ensure(has_audio, "录屏两路WAV没有实际音频后台")?;
            ensure(
                native.scale_factor().is_finite() && native.scale_factor() > 0.,
                "录屏结果窗DPI比例无效",
            )?;
            cx.notify();
            Ok(Input {
                handle: owner.native.get(),
                scale: native.scale_factor(),
            })
        })??;
        Ok((window, input))
    })??;
    let first_generation = window.update(cx, |view, _, _| {
        view.player.status.lock().unwrap().generation
    })?;
    let first = paused(window, owner, cx, first_generation, "initial").await?;
    ensure(
        first.pts == fixture.times[0] && first.position.is_zero(),
        "录屏窗口没有提交夹具首帧",
    )?;
    let center = (160. / input.scale, 120. / input.scale);
    input.click(center);
    let playing = wait(window, owner, cx, |observed| {
        observed.playing
            && !observed.pending
            && observed.clock.clock.advancing
            && observed.position >= Duration::from_millis(150)
            && observed.serial > first.serial
            && observed.pts > first.pts
    })
    .await?;
    input.click(center);
    let pause_generation = wait(window, owner, cx, |observed| {
        !observed.playing && observed.generation != playing.generation
    })
    .await?
    .generation;
    let stopped = paused(window, owner, cx, pause_generation, "native_pause").await?;
    ensure(
        moves.get() == 0 && stopped.generation != playing.generation,
        "窗口点击被误判为移动或未暂停",
    )?;

    // 结果窗只有点击播放/拖动；定位沿用宿主调用的真实Player接口，不声称工具栏已验。
    let target = window.update(cx, |view, _, cx| {
        let target = view.player.position().1.mul_f32(0.55);
        view.player.seek_ratio(0.55);
        cx.notify();
        target
    })?;
    let expected = fixture
        .times
        .iter()
        .copied()
        .filter(|pts| *pts <= target)
        .last()
        .ok_or("定位目标之前没有自有夹具帧")?;
    let seek_ready = wait(window, owner, cx, |observed| {
        !observed.playing
            && !observed.pending
            && observed.generation != stopped.generation
            && observed.pts == expected
            && observed.position == target
            && observed.clock.clock.media_position == target
    })
    .await?;
    let seeked = paused(window, owner, cx, seek_ready.generation, "seek").await?;
    let next_pts = fixture
        .times
        .iter()
        .copied()
        .find(|pts| *pts > seeked.pts)
        .ok_or("自有夹具没有可逐帧前进的后继")?;
    window.update(cx, |view, _, cx| {
        view.player.step_forward();
        cx.notify();
    })?;
    let step_ready = wait(window, owner, cx, |observed| {
        !observed.playing
            && !observed.pending
            && observed.generation != seeked.generation
            && observed.pts == next_pts
            && observed.position == next_pts
            && observed.clock.clock.media_position == next_pts
    })
    .await?;
    let stepped = paused(window, owner, cx, step_ready.generation, "step").await?;
    ensure(moves.get() == 0, "结果窗专项意外移动了真实结果位置")?;
    Ok(
        json!({"suite":"recording-preview-window","actual_recording_preview_constructor":true,
        "recording_view_toolbar_verified":false,"first_successful_paint":point(&first),
        "play":point(&playing),"pause":point(&stopped),"seek":point(&seeked),"step":point(&stepped),
        "seek_target_ms":target.as_millis(),"pause_stable_ms":225,
        "controls":{"play_pause":"native_preview_surface_click","seek_step":"existing_Player_API"},
        "wav_adapter":{"backend":"MF_AudioReader_open_for_preview","sources":["system","microphone"],
            "rate":48000,"channels":2,"pcm_bits":16,"source_frames_each":144000},
        "all_audio_fixture_zero_before_constructor":true,"output_muted":true,
        "initial_paused_seek_to_start_via_Player":true,
        "paint_submitted":true,"presented_verified":false,"audible_playback_verified":false,
        "desktop_pixels_read":false,"clipboard_untouched":true}),
    )
}

fn released(owner: &Owner) -> bool {
    let entity_gone = owner
        .entity
        .borrow()
        .as_ref()
        .is_none_or(|entity| entity.upgrade().is_none());
    let native_gone = owner.native.get() == 0
        || !unsafe { IsWindow(Some(HWND(owner.native.get() as *mut _))) }.as_bool();
    entity_gone
        && native_gone
        && owner
            .images
            .borrow()
            .iter()
            .all(|image| image.upgrade().is_none())
        && owner
            .workers
            .borrow()
            .as_ref()
            .is_none_or(PlaybackShutdownProbe::finished)
}

async fn close(owner: &Owner, cx: &mut AsyncApp) -> Result<Value> {
    if owner.native.get() == 0 {
        if let Some(window) = owner.window.get() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    }
    let posted = if owner.native.get() == 0 {
        Ok(())
    } else {
        unsafe {
            PostMessageW(
                Some(HWND(owner.native.get() as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .map_err(|error| error.to_string())
    };
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if released(owner) {
            posted?;
            return Ok(
                json!({"native_WM_CLOSE":true,"hwnd_entity_images_released":true,
                "video_worker_exited":true,"audio_worker_exited":true}),
            );
        }
        tick(cx).await;
    }
    // 失败也先销毁自有窗口再等后台；不能把兜底remove_window算作WM_CLOSE验收成功。
    if let Some(window) = owner.window.get() {
        let _ = window.update(cx, |_, window, _| window.remove_window());
    }
    let deadline = Instant::now() + WAIT;
    while !released(owner) && Instant::now() < deadline {
        tick(cx).await;
    }
    ensure(
        released(owner),
        "录屏结果窗失败收尾后仍持有窗口/图像/媒体后台",
    )?;
    Err("录屏结果窗WM_CLOSE没有在时限内释放窗口和两个媒体后台".into())
}

/// 只由独立ignored入口调用；实际结果窗而非自定义视频宿主。
pub(super) fn run() -> Result<()> {
    let fixture = Rc::new(Fixture::new()?);
    let input_fixture = fixture.clone();
    let outcome = Rc::new(RefCell::new(Some("录屏结果窗专项未完成".to_owned())));
    let finished = outcome.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx| {
            crate::theme::install(cx);
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
                let owner = Owner::default();
                let result = exercise(&input_fixture, &owner, cx).await;
                let cleanup = close(&owner, cx).await;
                *outcome.borrow_mut() = match (result, cleanup) {
                    (Ok(mut report), Ok(closed)) => {
                        report["close"] = closed;
                        println!("{report}");
                        None
                    }
                    (Err(error), Ok(_)) | (Ok(_), Err(error)) => Some(error.to_string()),
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
