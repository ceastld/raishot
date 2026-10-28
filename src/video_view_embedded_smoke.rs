//! 自有历史预览宿主验证真实提交及局部会话回收，不读取桌面像素。
use super::*;
use crate::video_view::HistoryVideoPreview;
use gpui::WeakEntity;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;

#[derive(Default)]
struct PreviewHost {
    preview: HistoryVideoPreview,
    title: String,
}

impl Render for PreviewHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let p = crate::theme::palette(cx);
        div()
            .relative()
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .bg(rgb(p.background))
            .child(self.preview.element(cx))
    }
}

#[derive(Default)]
struct PreviewLifecycle {
    window: Cell<Option<WindowHandle<PreviewHost>>>,
    native: Cell<usize>,
    entity: RefCell<Option<WeakEntity<VideoView>>>,
    images: RefCell<Vec<Weak<RenderImage>>>,
    workers: RefCell<Vec<PlaybackShutdownProbe>>,
}

fn remember_preview(host: &PreviewHost, lifecycle: &PreviewLifecycle, cx: &App) {
    let Some(entity) = host.preview.entity() else {
        return;
    };
    *lifecycle.entity.borrow_mut() = Some(entity.downgrade());
    let view = entity.read(cx);
    lifecycle.images.borrow_mut().extend(image_refs(view));
    if lifecycle.workers.borrow().is_empty() {
        if let Some(player) = &view.player {
            lifecycle.workers.borrow_mut().push(player.shutdown_probe());
        }
    }
}

fn resources_pending(lifecycle: &PreviewLifecycle) -> bool {
    lifecycle
        .entity
        .borrow()
        .as_ref()
        .is_some_and(|entity| entity.upgrade().is_some())
        || lifecycle
            .images
            .borrow()
            .iter()
            .any(|image| image.upgrade().is_some())
        || lifecycle
            .workers
            .borrow()
            .iter()
            .any(|probe| !probe.finished())
}

fn verify_host_title(window: &Window, expected: &str) -> Result<()> {
    let mut title = [0_u16; 256];
    let length = unsafe { GetWindowTextW(crate::gpui_capture::hwnd(window), &mut title) };
    ensure(
        length > 0 && String::from_utf16(&title[..length as usize])? == expected,
        "内嵌预览修改了宿主窗口标题",
    )
}

struct SubmittedPreview {
    serial: u64,
    pts: Duration,
    position: Duration,
    progress: [i32; 4],
    client: [i32; 2],
    scale: f32,
}

fn submitted_preview(
    window: WindowHandle<PreviewHost>,
    source: &Path,
    cx: &mut AsyncApp,
) -> Result<Option<SubmittedPreview>> {
    window.update(cx, |host, window, cx| -> Result<_> {
        verify_host_title(window, &host.title)?;
        let entity = host.preview.entity().ok_or("宿主没有创建内嵌预览实体")?;
        let view = entity.read(cx);
        if let Some(error) = &view.open_error {
            return Err(format!("内嵌预览预检失败：{error}").into());
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
        ensure(view.path.as_deref() == Some(source), "内嵌预览来源错误")?;
        ensure(
            !player.playing() && view.muted,
            "内嵌预览没有保持默认暂停与静音",
        )?;
        let Some(frame) = &view.submitted_frame else {
            return Ok(None);
        };
        ensure(
            frame.position.is_zero(),
            "内嵌预览首次提交的画面不是PTS零首帧",
        )?;
        ensure(
            player.submitted_frame_point() == Some((frame.serial, frame.position)),
            "内嵌真实paint提交未收到播放器ACK",
        )?;
        let position = player.position().0;
        ensure(
            position.is_zero() && !player.seeking(),
            "内嵌首帧实际位置已前进或仍在定位",
        )?;
        let bounds = view.seek_bounds.get().ok_or("内嵌进度条没有实际布局")?;
        let scale = window.scale_factor();
        let logical = [
            f32::from(bounds.left()),
            f32::from(bounds.top()),
            f32::from(bounds.right()),
            f32::from(bounds.bottom()),
        ];
        ensure(
            scale.is_finite() && scale > 0. && logical.iter().all(|value| value.is_finite()),
            "内嵌进度条物理坐标无效",
        )?;
        let progress = logical.map(|value| (value * scale).round() as i32);
        let mut client = RECT::default();
        unsafe { GetClientRect(crate::gpui_capture::hwnd(window), &mut client)? };
        ensure(
            progress[0] >= client.left
                && progress[1] >= client.top
                && progress[2] <= client.right
                && progress[3] <= client.bottom
                && progress[2] > progress[0]
                && progress[3] > progress[1],
            "内嵌进度条不在真实宿主客户区内",
        )?;
        Ok(Some(SubmittedPreview {
            serial: frame.serial,
            pts: frame.position,
            position,
            progress,
            client: [client.right - client.left, client.bottom - client.top],
            scale,
        }))
    })?
}

async fn exercise_preview(
    source: PathBuf,
    lifecycle: &Rc<PreviewLifecycle>,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let window = cx.update(|cx| -> Result<_> {
        crate::theme::install(cx);
        let bounds = Bounds::centered(None, size(px(300.), px(240.)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions::default()),
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                let title = tr!("视频预览宿主检查", "Video preview host check").to_owned();
                window.set_window_title(&title);
                cx.new(|_| PreviewHost {
                    title,
                    ..Default::default()
                })
            },
        )?;
        lifecycle.window.set(Some(window));
        window.update(cx, |host, window, cx| {
            let release_lifecycle = lifecycle.clone();
            cx.on_release(move |host, cx| {
                remember_preview(host, &release_lifecycle, cx);
                host.preview.clear(cx);
            })
            .detach();
            lifecycle
                .native
                .set(crate::gpui_capture::hwnd(window).0 as usize);
            host.preview
                .reconcile(Some(("fixture", source.as_path())), window, cx);
            remember_preview(host, lifecycle, cx);
            cx.notify();
        })?;
        Ok(window)
    })??;
    let deadline = Instant::now() + WAIT;
    let mut stable = None;
    let submitted = loop {
        if let Some(observed) = submitted_preview(window, &source, cx)? {
            match stable {
                Some((serial, position, since)) => {
                    ensure(
                        observed.serial == serial && observed.position == position,
                        "暂停首帧在稳定检查期间发生变化",
                    )?;
                    if Instant::now().duration_since(since) >= Duration::from_millis(200) {
                        break observed;
                    }
                }
                None => stable = Some((observed.serial, observed.position, Instant::now())),
            }
        }
        ensure(
            Instant::now() < deadline,
            "内嵌预览未提交并稳定保持默认首帧",
        )?;
        tick(cx).await;
    };
    window.update(cx, |host, window, cx| {
        remember_preview(host, lifecycle, cx);
        host.preview.clear(cx);
        cx.notify();
        window.refresh();
    })?;
    let deadline = Instant::now() + WAIT;
    while resources_pending(lifecycle) {
        ensure(
            unsafe { IsWindow(Some(HWND(lifecycle.native.get() as *mut _))) }.as_bool(),
            "清除预览时宿主窗口被关闭，不能证明局部会话回收",
        )?;
        ensure(
            Instant::now() < deadline,
            "清除预览后实体、图像或媒体工作线程未回收",
        )?;
        window.update(cx, |host, window, _| -> Result<()> {
            verify_host_title(window, &host.title)?;
            window.refresh();
            Ok(())
        })??;
        tick(cx).await;
    }
    ensure(
        unsafe { IsWindow(Some(HWND(lifecycle.native.get() as *mut _))) }.as_bool(),
        "预览回收后宿主窗口已不存在",
    )?;
    window.update(cx, |host, window, _| verify_host_title(window, &host.title))??;
    ensure(
        !lifecycle.workers.borrow().is_empty(),
        "没有取得真实播放器退出探针",
    )?;
    Ok(serde_json::json!({
        "ok":true,"suite":"history-video-preview-window","silent_fixture":true,
        "requested_logical_size":[300,240],"actual_client_physical":submitted.client,
        "scale":submitted.scale,"progress_physical":submitted.progress,
        "submitted_serial":submitted.serial,"submitted_pts_ms":submitted.pts.as_millis(),"player_ack_matches":true,
        "muted_paused_stable_ms":200,"host_title_unchanged":true,
        "host_survived_preview_clear":true,"preview_entity_images_workers_released":true,
        "presented_verified":false,"desktop_pixels_read":false,"clipboard_untouched":true,
    }))
}

async fn cleanup_preview(lifecycle: &PreviewLifecycle, cx: &mut AsyncApp) -> Result<()> {
    if let Some(window) = lifecycle.window.get() {
        let _ = window.update(cx, |host, window, cx| {
            remember_preview(host, lifecycle, cx);
            host.preview.clear(cx);
            window.remove_window();
        });
    }
    let deadline = Instant::now() + WAIT;
    while resources_pending(lifecycle)
        || (lifecycle.native.get() != 0
            && unsafe { IsWindow(Some(HWND(lifecycle.native.get() as *mut _))) }.as_bool())
    {
        ensure(Instant::now() < deadline, "宿主收尾时窗口或媒体资源未退出")?;
        tick(cx).await;
    }
    Ok(())
}

fn run_preview() -> Result<()> {
    let fixtures = Fixtures::new()?;
    let paths = write_fixtures(&fixtures.root, Scenario::Silent)?;
    let source = paths.first().ok_or("缺少自有静音视频夹具")?.clone();
    let outcome = Rc::new(RefCell::new(Some("历史视频预览专项未完成".to_owned())));
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
            let outcome = outcome.clone();
            cx.spawn(async move |cx| {
                let lifecycle = Rc::new(PreviewLifecycle::default());
                let result = exercise_preview(source, &lifecycle, cx).await;
                let cleanup = cleanup_preview(&lifecycle, cx).await;
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
#[ignore = "显式单独运行自有300×240历史视频预览检查；需交互桌面，不读取桌面像素"]
fn native_history_video_preview_lifecycle_smoke() {
    run_preview().expect("历史视频内嵌预览原生检查失败");
}
