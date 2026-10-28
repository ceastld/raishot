//! 自有 SettingsView 的真实历史详情接线；只用内存条目和无声夹具，不注册主窗口单例。
use super::super::{main_page::MainPage, main_pins, translation, SettingsTab};
use super::*;
use crate::{
    media_foundation::{MfRuntime, Mp4Writer},
    recording_preview::PlaybackShutdownProbe,
    video_view::VideoView,
    Result,
};
use gpui::{
    size, AppContext, Application, AsyncApp, Bounds, Render, WeakEntity, WindowBounds,
    WindowHandle, WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Weak,
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        IsIconic, IsWindow, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
        SW_MINIMIZE, SW_RESTORE,
    },
};

const WAIT: Duration = Duration::from_secs(20);
const FIRST: &str = "smoke:first";
const SECOND: &str = "smoke:second";
const OTHER: &str = "smoke:other";

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

struct Fixtures {
    root: PathBuf,
    paths: [PathBuf; 2],
    previous_experimental: Option<OsString>,
}

impl Fixtures {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustshot-history-page-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root)?;
        let previous_experimental = std::env::var_os("RUSTSHOT_EXPERIMENTAL_MEDIA");
        std::env::remove_var("RUSTSHOT_EXPERIMENTAL_MEDIA");
        let fixture = Self {
            paths: [root.join("first.mp4"), root.join("second.mp4")],
            root,
            previous_experimental,
        };
        let _runtime = MfRuntime::new()?;
        for (index, path) in fixture.paths.iter().enumerate() {
            let mut writer = Mp4Writer::new_legacy_for_benchmark(path, 64, 48, 10, 200_000, false)?;
            let mut pixels = vec![0_u8; 64 * 48 * 4];
            for frame in 0..20_u8 {
                let color = if index == 0 {
                    [24, 40 + frame, 180, 255]
                } else {
                    [24, 180, 40 + frame, 255]
                };
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&color);
                }
                writer.write_video(
                    &pixels,
                    64,
                    Duration::from_millis(u64::from(frame) * 100),
                    Duration::from_millis(100),
                )?;
            }
            writer.finish()?;
        }
        Ok(fixture)
    }

    fn items(&self) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        for (key, path) in [FIRST, SECOND].into_iter().zip(&self.paths) {
            let bytes = fs::metadata(path)?.len();
            items.push(Item {
                key: key.into(),
                created: SystemTime::now(),
                title: key.into(),
                size: Some((64, 48)),
                bytes: Some(bytes),
                source: Source::Recording(recordings::Entry {
                    path: path.clone(),
                    name: path.file_name().unwrap().to_string_lossy().into_owned(),
                    bytes,
                    modified: None,
                    info: crate::recording_library_metadata::VideoInfo {
                        width: Some(64),
                        height: Some(48),
                        duration: Some(Duration::from_secs(2)),
                    },
                }),
            });
        }
        items.push(Item {
            key: OTHER.into(),
            created: SystemTime::now(),
            title: "memory text pin".into(),
            size: None,
            bytes: None,
            source: Source::Pin {
                id: "memory-only".into(),
                image: None,
                text: true,
                preview_text: None,
                state: PinState::Closed,
                group: String::new(),
            },
        });
        Ok(items)
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        match &self.previous_experimental {
            Some(value) => std::env::set_var("RUSTSHOT_EXPERIMENTAL_MEDIA", value),
            None => std::env::remove_var("RUSTSHOT_EXPERIMENTAL_MEDIA"),
        }
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&self.root);
    }
}

/// 与 open_unique 分开：所有草稿来自内存，不安装单例、激活回调或真实历史扫描。
fn memory_settings(
    items: Vec<Item>,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> SettingsView {
    let preferences = Preferences::default();
    let positions = Rc::new(RefCell::new(HashMap::new()));
    let save_settings = cx
        .new(|cx| crate::settings_save::SaveSettings::new_in_memory(window, positions.clone(), cx));
    let mut history = HistoryPage::new(window, cx);
    history.items = items;
    history.selected = Some(FIRST.into());
    history.scanned_at = Some(Instant::now());
    SettingsView::watch_content(window, cx);
    SettingsView {
        quick_actions: crate::settings_quick_actions::QuickActionSettings::new_in_memory(
            window, cx,
        ),
        tab: SettingsTab::default(),
        toolbar_config: crate::toolbar_config::ToolbarConfig::default(),
        autostart: Err("memory-only history probe".into()),
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
        media_popup: super::super::main_media_popup::MediaPopup::default(),
        _instance: None,
    }
}

struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().bg(rgb(crate::theme::palette(cx).background))
    }
}

#[derive(Default)]
struct Lifecycle {
    window: Cell<Option<WindowHandle<SettingsView>>>,
    keep_alive: Cell<Option<WindowHandle<KeepAlive>>>,
    native: Cell<usize>,
    observations: RefCell<Vec<Observed>>,
}

#[derive(Clone)]
struct Observed {
    entity: WeakEntity<VideoView>,
    images: Vec<Weak<RenderImage>>,
    worker: PlaybackShutdownProbe,
    serial: u64,
    pts: Duration,
}

impl Observed {
    fn released(&self, graphics: bool) -> bool {
        self.worker.finished()
            && (!graphics
                || (self.entity.upgrade().is_none()
                    && self.images.iter().all(|image| image.upgrade().is_none())))
    }
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}

async fn wait_for<T>(
    cx: &mut AsyncApp,
    label: &str,
    mut inspect: impl FnMut(&mut AsyncApp) -> Result<Option<T>>,
) -> Result<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = inspect(cx)? {
            return Ok(value);
        }
        ensure(Instant::now() < deadline, format!("{label}超时"))?;
        tick(cx).await;
    }
}

async fn ready(
    window: WindowHandle<SettingsView>,
    path: &Path,
    life: &Lifecycle,
    cx: &mut AsyncApp,
) -> Result<Observed> {
    let observed = wait_for(cx, "真实历史详情首帧提交", |cx| {
        window.update(cx, |view, _, cx| -> Result<_> {
            ensure(view._instance.is_none(), "测试注册了真实主窗口单例")?;
            ensure(
                view.history.items.len() == 3 && view.history.pending.is_empty(),
                "内存历史被修改或排队删除",
            )?;
            let Some(probe) = view.history.video_preview.probe(cx) else {
                return Ok(None);
            };
            if let Some(error) = probe.error {
                return Err(error.into());
            }
            let (Some(submitted), Some(worker)) = (probe.submitted, probe.worker) else {
                return Ok(None);
            };
            ensure(probe.source.as_deref() == Some(path), "详情提交了错误来源")?;
            ensure(
                probe.acknowledged == Some(submitted),
                "历史详情paint没有真实Player ACK",
            )?;
            ensure(
                probe.muted && probe.playing == Some(false),
                "历史详情不是暂停且静音",
            )?;
            ensure(
                submitted.1.is_zero() && probe.position == Some(Duration::ZERO),
                "暂停首帧或进度提前推进",
            )?;
            Ok(Some(Observed {
                entity: probe.entity,
                images: probe.images,
                worker,
                serial: submitted.0,
                pts: submitted.1,
            }))
        })?
    })
    .await?;
    // 每次活动会话一取得真实回执就留下弱引用与退出标记，失败分支也等待它退出。
    life.observations.borrow_mut().push(observed.clone());
    Ok(observed)
}

async fn released(
    window: WindowHandle<SettingsView>,
    old: &Observed,
    graphics: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    wait_for(cx, "旧历史预览资源退出", |cx| {
        window.update(cx, |_, window, _| window.refresh())?;
        Ok(old.released(graphics).then_some(()))
    })
    .await
}

async fn cleared(window: WindowHandle<SettingsView>, cx: &mut AsyncApp) -> Result<()> {
    wait_for(cx, "历史详情清理当前会话", |cx| {
        window.update(cx, |view, _, cx| -> Result<Option<()>> {
            Ok(view.history.video_preview.probe(cx).is_none().then_some(()))
        })?
    })
    .await
}

fn focus_paused_preview(window: WindowHandle<SettingsView>, cx: &mut AsyncApp) -> Result<()> {
    window.update(cx, |view, window, cx| -> Result<()> {
        ensure(
            view.history.video_preview.toggle(window, cx),
            "缺少可获焦的历史预览",
        )?;
        ensure(
            view.history.video_preview.toggle(window, cx),
            "历史预览未恢复暂停",
        )?;
        ensure(
            view.history.video_preview.has_focus(window, cx),
            "子预览未实际持有焦点",
        )?;
        ensure(
            view.history
                .video_preview
                .probe(cx)
                .is_some_and(|probe| probe.playing == Some(false)),
            "获焦后的预览没有暂停",
        )
    })??;
    Ok(())
}

fn history_focused(window: WindowHandle<SettingsView>, cx: &mut AsyncApp) -> Result<()> {
    window.update(cx, |view, window, _| {
        ensure(
            view.history.focus.is_focused(window),
            "释放预览后未交回历史焦点",
        )
    })??;
    Ok(())
}

fn restore_memory_history(window: WindowHandle<SettingsView>, cx: &mut AsyncApp) -> Result<()> {
    window.update(cx, |view, _, cx| {
        // 只恢复本测试的内存页；不调用刷新入口，慢机器也不会触发真实库扫描。
        view.page = MainPage::History;
        view.history.detail = true;
        view.history.filter = Filter::All;
        view.history
            .search
            .update(cx, |field, cx| field.set_text(String::new(), cx));
        view.history_select(FIRST.into(), cx);
        cx.notify();
    })?;
    Ok(())
}

async fn exercise(
    items: Vec<Item>,
    paths: [PathBuf; 2],
    life: &Lifecycle,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let window = cx.update(|cx| -> Result<_> {
        // 直接安装系统主题内存状态，不调用会轮询用户Preferences的theme::install。
        crate::theme::set_mode(crate::theme::ThemeMode::System, cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(740.)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&format!(
                    "{} · {}",
                    crate::brand::NAME,
                    tr!("历史预览接线检查", "History preview check")
                ));
                cx.new(|cx| memory_settings(items, window, cx))
            },
        )?;
        life.window.set(Some(window));
        window.update(cx, |view, window, cx| {
            life.native
                .set(crate::gpui_capture::hwnd(window).0 as usize);
            window.focus(&view.history.focus);
            cx.notify();
        })?;
        Ok(window)
    })??;
    let first = ready(window, &paths[0], life, cx).await?;
    window.update(cx, |view, _, cx| view.history_select(FIRST.into(), cx))?;
    let same = ready(window, &paths[0], life, cx).await?;
    ensure(
        first.entity.upgrade() == same.entity.upgrade(),
        "相同历史选择重建了预览",
    )?;
    ensure(
        first.serial == same.serial && first.pts == same.pts,
        "暂停详情画面推进",
    )?;
    focus_paused_preview(window, cx)?;
    let escape = gpui::Keystroke::parse("escape").map_err(|error| error.to_string())?;
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(escape, cx);
    })?;
    window.update(cx, |view, window, cx| -> Result<()> {
        ensure(view.history.selected.is_none(), "Esc未取消历史选择")?;
        ensure(
            view.history.video_preview.probe(cx).is_none(),
            "Esc未移除历史预览",
        )?;
        ensure(
            view.history.focus.is_focused(window),
            "Esc未将焦点交回历史列表",
        )
    })??;
    released(window, &first, true, cx).await?;
    restore_memory_history(window, cx)?;
    let first = ready(window, &paths[0], life, cx).await?;
    window.update(cx, |view, _, cx| view.history_select(SECOND.into(), cx))?;
    released(window, &first, true, cx).await?;
    life.observations.borrow_mut().push(first);
    let second = ready(window, &paths[1], life, cx).await?;

    // 通过真实TextField::insert发出Changed，让生产订阅及history_body应用搜索过滤。
    window.update(cx, |view, window, cx| {
        window.focus(&view.history.search.read(cx).focus_handle);
        view.history.search.update(cx, |field, cx| {
            field.insert("no-match-history-probe", window, cx)
        });
    })?;
    cleared(window, cx).await?;
    released(window, &second, true, cx).await?;
    window.update(cx, |view, window, cx| {
        ensure(
            view.history.search.read(cx).focus_handle.is_focused(window),
            "释放旧预览夺走了搜索焦点",
        )
    })??;
    life.observations.borrow_mut().push(second);
    restore_memory_history(window, cx)?;
    let current = ready(window, &paths[0], life, cx).await?;
    window.update(cx, |view, _, cx| {
        view.history.filter = Filter::Shots;
        cx.notify();
    })?;
    cleared(window, cx).await?;
    released(window, &current, true, cx).await?;
    life.observations.borrow_mut().push(current);

    restore_memory_history(window, cx)?;
    let current = ready(window, &paths[0], life, cx).await?;
    window.update(cx, |view, _, cx| view.history_select(OTHER.into(), cx))?;
    cleared(window, cx).await?;
    released(window, &current, true, cx).await?;
    life.observations.borrow_mut().push(current);

    restore_memory_history(window, cx)?;
    let current = ready(window, &paths[0], life, cx).await?;
    window.update(cx, |view, _, cx| {
        view.history.detail = false;
        cx.notify();
    })?;
    cleared(window, cx).await?;
    released(window, &current, true, cx).await?;
    life.observations.borrow_mut().push(current);

    restore_memory_history(window, cx)?;
    let current = ready(window, &paths[0], life, cx).await?;
    // 设置页只渲染内存中的偏好；Help的生产render会读取Preferences，因此不进入Help。
    window.update(cx, |view, _, cx| view.show_page(MainPage::Settings, cx))?;
    cleared(window, cx).await?;
    released(window, &current, true, cx).await?;
    life.observations.borrow_mut().push(current);

    restore_memory_history(window, cx)?;
    let current = ready(window, &paths[0], life, cx).await?;
    let hwnd = HWND(life.native.get() as *mut _);
    focus_paused_preview(window, cx)?;
    let scale = window.update(cx, |_, window, _| window.scale_factor())?;
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            (780. * scale) as i32,
            (620. * scale) as i32,
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
        )?;
    }
    cleared(window, cx).await?;
    released(window, &current, true, cx).await?;
    history_focused(window, cx)?;
    life.observations.borrow_mut().push(current);
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            (1280. * scale) as i32,
            (740. * scale) as i32,
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
        )?;
    }
    let current = ready(window, &paths[0], life, cx).await?;
    focus_paused_preview(window, cx)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
    wait_for(cx, "自有主窗口最小化", |_| {
        Ok(unsafe { IsIconic(hwnd) }.as_bool().then_some(()))
    })
    .await?;
    // 不手调history_preview_tick：这里必须由与open_unique共用的生产watch_content清理。
    cleared(window, cx).await?;
    released(window, &current, false, cx).await?;
    ensure(
        unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "最小化清理关闭了主窗口",
    )?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    let closing = ready(window, &paths[0], life, cx).await?;
    released(window, &current, true, cx).await?;
    history_focused(window, cx)?;
    life.observations.borrow_mut().push(current);
    life.observations.borrow_mut().push(closing);
    // 让生产字段Drop/release收尾，不预先调用adapter.clear来代替关窗路径。
    window.update(cx, |_, window, _| window.remove_window())?;
    wait_for(cx, "关窗后真实历史预览资源退出", |_| {
        Ok((!unsafe { IsWindow(Some(hwnd)) }.as_bool()
            && life
                .observations
                .borrow()
                .iter()
                .all(|value| value.released(true)))
        .then_some(()))
    })
    .await?;
    Ok(serde_json::json!({
        "ok":true,"suite":"settings_view_history_preview_integration",
        "real_settings_render":true,"real_history_reconcile":true,"real_host_poll":true,
        "same_selection_reused":true,"source_switch_retired":true,
        "gpui_escape_cleared_and_returned_focus":true,
        "narrow_and_minimize_restore_returned_focus":true,"search_focus_preserved":true,
        "native_escape_message_verified":false,
        "search_and_type_filters_cleared":true,"non_video_and_hidden_detail_cleared":true,
        "leave_page_and_narrow_layout_cleared":true,"minimize_workers_exited":true,
        "restored_graphics_released":true,"close_workers_entities_images_released":true,
        "history_items_in_memory":3,"user_history_scanned":false,"user_settings_loaded":false,
        "singleton_registered":false,"audio_output":false,"desktop_pixels_read":false,
        "submitted_ack_verified":true,"presented_verified":false,
    }))
}

async fn cleanup(life: &Lifecycle, cx: &mut AsyncApp) -> Result<()> {
    if let Some(window) = life.window.get() {
        let _ = window.update(cx, |view, window, cx| {
            if let Some(probe) = view.history.video_preview.probe(cx) {
                if let Some(worker) = probe.worker {
                    life.observations.borrow_mut().push(Observed {
                        entity: probe.entity,
                        images: probe.images,
                        worker,
                        serial: 0,
                        pts: Duration::ZERO,
                    });
                }
            }
            view.history_clear_video_preview(cx);
            window.remove_window();
        });
    }
    wait_for(cx, "测试失败收尾", |_| {
        let alive = life.native.get() != 0
            && unsafe { IsWindow(Some(HWND(life.native.get() as *mut _))) }.as_bool();
        Ok((!alive
            && life
                .observations
                .borrow()
                .iter()
                .all(|value| value.released(true)))
        .then_some(()))
    })
    .await
}

#[test]
#[ignore = "显式单独运行真实SettingsView历史内嵌预览；自有无声夹具和内存历史，不读取用户设置或注册单例"]
fn native_main_history_video_preview_integration_smoke() {
    let fixture = Fixtures::new().expect("创建自有无声夹具");
    let items = fixture.items().expect("构造内存历史条目");
    let paths = fixture.paths.clone();
    let result = Rc::new(RefCell::new(Some("主界面历史预览检查未完成".to_owned())));
    let completed = result.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx| {
            crate::theme::set_mode(crate::theme::ThemeMode::System, cx);
            let life = Rc::new(Lifecycle::default());
            match cx.open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| KeepAlive),
            ) {
                Ok(window) => life.keep_alive.set(Some(window)),
                Err(error) => {
                    *result.borrow_mut() = Some(error.to_string());
                    cx.quit();
                    return;
                }
            }
            cx.spawn(async move |cx| {
                let observed = exercise(items, paths, &life, cx).await;
                let closed = cleanup(&life, cx).await;
                *result.borrow_mut() = match (observed, closed) {
                    (Ok(report), Ok(())) => {
                        println!("{report}");
                        None
                    }
                    (Err(error), Ok(())) | (Ok(_), Err(error)) => Some(error.to_string()),
                    (Err(error), Err(close)) => Some(format!("{error}；收尾失败：{close}")),
                };
                let _ = cx.update(|cx| {
                    if let Some(window) = life.keep_alive.get() {
                        let _ = window.update(cx, |_, window, _| window.remove_window());
                    }
                    cx.quit();
                });
            })
            .detach();
        });
    let error = completed.borrow_mut().take();
    if let Some(error) = error {
        panic!("{error}");
    }
}
