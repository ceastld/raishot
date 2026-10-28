//! 真实长截图操作条裁剪 / 保存 / 贴图的排空交接；只使用自有像素和 HWND。
use crate::{
    capture::Image,
    capture_view::CaptureView,
    geometry::ScreenRect,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    long_capture_runtime::LongPhase,
    long_capture_smoke::{Fixture, FixturePattern},
    long_capture_view::LongCaptureView,
    long_pipeline_smoke::{
        ensure, options, oracle, oracle_image, settled, MatchLatch, ReleaseLatch, Report, HEIGHT,
        WAIT, WIDTH,
    },
    pin_view::{PinAction, PinView},
    save_panel::SavePanel,
    save_store::SaveStore,
    stitch::ScrollAxis,
    Result,
};
use gpui::{
    div, App, AppContext, Application, AsyncApp, Context, Render, Window, WindowHandle,
    WindowOptions,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GetWindowDisplayAffinity, GetWindowThreadProcessId, IsWindow, WDA_EXCLUDEFROMCAPTURE,
    },
};
struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
async fn step(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(5))
        .await;
}
async fn wait_panel(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    predicate: impl Fn(&crate::long_capture_runtime::LongSnapshot, &serde_json::Value) -> bool,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let end = Instant::now() + WAIT;
    loop {
        let (meta, probe) = panel
            .update(cx, |v, _, _| -> Result<_> {
                Ok((
                    v.snapshot(),
                    serde_json::to_value(
                        v.session
                            .as_ref()
                            .ok_or("自有面板采集器未启动")?
                            .pipeline_probe(),
                    )?,
                ))
            })
            .map_err(|e| e.to_string())??;
        if predicate(&meta, &probe) {
            return Ok(probe);
        }
        ensure(
            meta.phase != LongPhase::Failed,
            &format!("{label}：{}", meta.status),
        )?;
        ensure(
            Instant::now() < end,
            &format!("{label}超时：{} {probe}", meta.status),
        )?;
        step(cx).await;
    }
}
fn post_click(input: Input, p: (f32, f32)) -> Result<()> {
    let h = HWND(input.handle as *mut _);
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut process));
    }
    ensure(
        process == std::process::id() && unsafe { IsWindow(Some(h)) }.as_bool(),
        "按钮HWND不属于自有进程",
    )?;
    input.click(p);
    Ok(())
}
async fn panel_button(
    panel: WindowHandle<LongCaptureView>,
    id: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let value = panel
            .update(cx, |v, w, _| {
                v.ui_positions.borrow().get(id).copied().map(|p| {
                    (
                        Input {
                            handle: hwnd(w).0 as usize,
                            scale: w.scale_factor(),
                        },
                        p,
                    )
                })
            })
            .map_err(|e| e.to_string())?;
        if let Some((input, p)) = value {
            return post_click(input, p);
        }
        ensure(Instant::now() < end, &format!("没有实际面板按钮{id}"))?;
        step(cx).await;
    }
}
async fn wait_closed(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        if panel.update(cx, |_, _, _| ()).is_err() {
            break;
        }
        ensure(Instant::now() < end, "生产输出未回收长截图面板")?;
        step(cx).await;
    }
    ensure(
        crate::long_capture_pipeline::active_capture_threads() == 0
            && crate::long_capture_runtime::active_workers() == 0,
        "面板关闭后两端线程残留",
    )
}
async fn verify(region: ScreenRect, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    std::fs::create_dir_all(report.root.join("output"))?;
    let mut store = SaveStore::load();
    store.config.directory = report.root.join("output");
    store.config.filename_template = "pipeline-owned".into();
    store.save()?;
    let mut cases = vec![];
    for action in ["long-crop", "long-save", "long-pin"] {
        let mut fixture =
            Fixture::new_owned(region, ScrollAxis::Vertical, (0, 0), FixturePattern::Noise)?;
        let latch = Arc::new(MatchLatch::default());
        let release = ReleaseLatch(latch.clone());
        let gate = fixture.capture_gate_with_own_overlays()?;
        let hook = latch.hook();
        let panel = cx
            .update(|cx| LongCaptureView::open_guarded(region, options(100), gate, Some(hook), cx))
            .map_err(|e| e.to_string())??;
        let end = Instant::now() + WAIT;
        while !panel
            .update(cx, |v, _, _| v.session.is_some())
            .map_err(|e| e.to_string())?
        {
            ensure(Instant::now() < end, "自有GPUI面板初始化未启动采集")?;
            step(cx).await;
        }
        wait_panel(
            panel,
            "实际UI首帧",
            |m, _| m.height == HEIGHT && m.result_available,
            cx,
        )
        .await?;
        // 按需截取真实 GPUI 帧，不受操作条排除桌面采集的影响。
        if action == "long-crop" && std::env::var_os("RUSTSHOT_LONG_WORKFLOW_REPORT").is_some() {
            let window = panel.update(cx, |_, w, _| w.window_handle())?;
            crate::long_workflow_smoke::save_window_frame(window, "toolbar-c.png", None, cx)
                .await?;
            panel_button(panel, "long-settings", cx).await?;
            panel_button(panel, "long-fixed-manual", cx).await?;
            crate::long_workflow_smoke::save_window_frame(
                window,
                "toolbar-c-settings.png",
                None,
                cx,
            )
            .await?;
            panel.update(cx, |v, w, _| -> Result<()> {
                let viewport = w.viewport_size();
                let positions = v.ui_positions.borrow();
                for id in ["long-auto-adaptive", "long-fixed-start", "long-fixed-end"] {
                    let &(x, y) = positions.get(id).ok_or_else(|| format!("缺少操作：{id}"))?;
                    ensure(
                        x > 0.
                            && y > 0.
                            && x < f32::from(viewport.width)
                            && y + 18. < f32::from(viewport.height),
                        &format!("参数被工具栏裁切：{id}"),
                    )?;
                }
                Ok(())
            })??;
            panel_button(panel, "long-settings", cx).await?;
            wait_panel(
                panel,
                "设置关闭后采集恢复",
                |m, _| settled(m) && m.phase == LongPhase::Running,
                cx,
            )
            .await?;
        }
        panel
            .update(cx, |v, _, _| -> Result<()> {
                let s = v.session.as_mut().ok_or("无采集器")?;
                s.set_smoke_coalescing(false);
                s.set_paused(true)
            })
            .map_err(|e| e.to_string())??;
        wait_panel(
            panel,
            "准备真实暂停",
            |m, p| settled(m) && m.phase == LongPhase::Paused && p["queue_depth"] == 0,
            cx,
        )
        .await?;
        fixture.wheel(ScrollAxis::Vertical, 1)?;
        latch.arm();
        panel
            .update(cx, |v, _, _| {
                v.session.as_mut().ok_or("无采集器")?.set_paused(false)
            })
            .map_err(|e| e.to_string())??;
        wait_panel(panel, "真实consumer匹配停点", |_, _| latch.entered(), cx).await?;
        fixture.wheel(ScrollAxis::Vertical, 1)?;
        let after_present = panel
            .update(cx, |v, _, _| {
                v.session
                    .as_ref()
                    .expect("已启动")
                    .pipeline_probe()
                    .last_issued_sequence
            })
            .map_err(|e| e.to_string())?;
        let queued = wait_panel(
            panel,
            "尾帧实际采集而尚未处理",
            |_, p| {
                p["reads"].as_array().is_some_and(|r| {
                    r.iter()
                        .any(|v| v["sequence"].as_u64().unwrap_or(0) > after_present)
                }) && p["queue_depth"].as_u64().unwrap_or(0) > 0
            },
            cx,
        )
        .await?;
        let panel_native = panel
            .update(cx, |_, w, _| -> Result<usize> {
                let h = hwnd(w);
                let mut affinity = 0;
                unsafe {
                    GetWindowDisplayAffinity(h, &mut affinity)?;
                }
                ensure(affinity == WDA_EXCLUDEFROMCAPTURE.0, "真实面板未排除采集")?;
                Ok(h.0 as usize)
            })
            .map_err(|e| e.to_string())??;
        panel_button(panel, action, cx).await?;
        let barrier = wait_panel(
            panel,
            "实际按钮已请求排空",
            |m, p| m.requested_generation != m.generation && p["drain_fence"].is_object(),
            cx,
        )
        .await?;
        latch.release();
        // Pin 的异步输出可以先关闭采集面板，最终原图才是可靠观察点。
        let drained = if action == "long-pin" {
            serde_json::json!({"intermediate_ack_observed":false,"verification":"final-pin-pixels-and-thread-exit"})
        } else {
            let value = wait_panel(
                panel,
                "实际按钮等待尾帧ACK",
                |m, p| {
                    settled(m)
                        && m.phase == LongPhase::Paused
                        && m.height == HEIGHT + 64
                        && p["producer_quiesced"] == true
                        && p["queue_depth"] == 0
                },
                cx,
            )
            .await?;
            let snapshot = panel
                .update(cx, |v, _, _| {
                    v.stitch_snapshot().ok_or("UI排空没有共享快照")
                })
                .map_err(|e| e.to_string())??;
            oracle(&snapshot, HEIGHT + 64)?;
            drop(snapshot);
            value
        };
        let checks = WIDTH as usize * (HEIGHT + 64) as usize;
        if action == "long-crop" {
            // 限额内裁剪进入同窗剪辑宿主，继续采集保留原会话与运行意图。
            let end = Instant::now() + WAIT;
            let editor = loop {
                if let Some(w) = cx
                    .update(|cx| {
                        cx.windows()
                            .into_iter()
                            .find_map(|w| w.downcast::<CaptureView>())
                    })
                    .map_err(|e| e.to_string())?
                {
                    break w;
                }
                ensure(Instant::now() < end, "实际裁剪没有打开同窗长图编辑器")?;
                step(cx).await;
            };
            crate::long_editor_smoke::prepare_clip_input(editor, "pipeline-crop", cx).await?;
            editor
                .update(cx, |v, _, _| -> Result<()> {
                    let clip = v.editor.long_clip().ok_or("生产 Crop 缺少剪辑模型")?;
                    ensure(
                        clip.source_size() == (WIDTH, HEIGHT + 64)
                            && clip.axis() == ScrollAxis::Vertical
                            && clip.removed().is_empty(),
                        "生产 Crop 原图尺寸或初始剪辑错误",
                    )?;
                    oracle_image(&v.selected_content_image()?, HEIGHT + 64)
                })
                .map_err(|e| e.to_string())??;
            let end = Instant::now() + WAIT;
            loop {
                let click = editor
                    .update(cx, |v, w, _| {
                        v.ui_positions
                            .borrow()
                            .get("long-clip-return")
                            .copied()
                            .map(|p| {
                                (
                                    Input {
                                        handle: hwnd(w).0 as usize,
                                        scale: w.scale_factor(),
                                    },
                                    p,
                                )
                            })
                    })
                    .map_err(|e| e.to_string())?;
                if let Some((input, p)) = click {
                    crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
                    post_click(input, p)?;
                    break;
                }
                ensure(Instant::now() < end, "同窗继续采集按钮未绘制")?;
                step(cx).await;
            }
            wait_panel(
                panel,
                "同窗长图编辑器返回恢复原运行意图",
                |m, _| m.phase == LongPhase::Running && m.height == HEIGHT + 64,
                cx,
            )
            .await?;
            panel_button(panel, "long-close", cx).await?;
            wait_closed(panel, cx).await?;
        } else if action == "long-save" {
            let end = Instant::now() + WAIT;
            let save = loop {
                if let Some(w) = cx
                    .update(|cx| {
                        cx.windows()
                            .into_iter()
                            .find_map(|w| w.downcast::<SavePanel>())
                    })
                    .map_err(|e| e.to_string())?
                {
                    break w;
                }
                ensure(Instant::now() < end, "实际Save按钮没有打开SavePanel")?;
                step(cx).await;
            };
            let end = Instant::now() + WAIT;
            loop {
                let click = save
                    .update(cx, |v, w, _| {
                        v.ui_positions
                            .borrow()
                            .get("save-submit")
                            .copied()
                            .filter(|_| !v.encoding_pending)
                            .map(|p| {
                                (
                                    Input {
                                        handle: hwnd(w).0 as usize,
                                        scale: w.scale_factor(),
                                    },
                                    p,
                                )
                            })
                    })
                    .map_err(|e| e.to_string())?;
                if let Some((input, p)) = click {
                    post_click(input, p)?;
                    break;
                }
                ensure(Instant::now() < end, "自有保存按钮未就绪")?;
                step(cx).await;
            }
            wait_closed(panel, cx).await?;
            let image = crate::export::read_image(&report.root.join("output/pipeline-owned.png"))?;
            oracle_image(&image, HEIGHT + 64)?;
        } else {
            let end = Instant::now() + WAIT;
            let pin = loop {
                if let Some(w) = cx
                    .update(|cx| {
                        cx.windows()
                            .into_iter()
                            .find_map(|w| w.downcast::<PinView>())
                    })
                    .map_err(|e| e.to_string())?
                {
                    break w;
                }
                ensure(Instant::now() < end, "实际Pin没有创建贴图")?;
                step(cx).await;
            };
            pin.update(cx, |v, _, _| -> Result<()> {
                let image = Image {
                    width: v.original_size.0,
                    height: v.original_size.1,
                    bgra: v.original.as_bytes(0).ok_or("贴图无底图")?.to_vec(),
                    readback_ms: 0.,
                };
                oracle_image(&image, HEIGHT + 64)
            })
            .map_err(|e| e.to_string())??;
            wait_closed(panel, cx).await?;
            pin.update(cx, |v, w, cx| v.perform(PinAction::Close, w, cx))
                .map_err(|e| e.to_string())?;
        }
        // GPUI 先移除实体，再由 WindowsWindow::drop 的异步任务 DestroyWindow。
        let end = Instant::now() + WAIT;
        while unsafe { IsWindow(Some(HWND(panel_native as *mut _))) }.as_bool() {
            ensure(Instant::now() < end, "真实面板HWND未销毁")?;
            step(cx).await;
        }
        fixture.close()?;
        drop(release);
        let value = serde_json::json!({"action":action,"queued_before_request":queued,"pending_barrier":barrier,"drained":drained,
            "dimensions":[WIDTH,HEIGHT+64],"pixel_checks":checks,"native_posted_button":true,"full_desktop_freeze_used":false,
            "gate":fixture.capture_probe_snapshot(),"producer_threads_after_close":crate::long_capture_pipeline::active_capture_threads()});
        report.record("real-ui-output-drain", value.clone())?;
        cases.push(value);
    }
    report.groups.push(serde_json::json!({"name":"drain","ok":true,"detail":{"cases":cases,"production_entries":3,"clipboard_copy_tested":false}}));
    report.save(false, None)
}
pub(crate) fn run(region: ScreenRect, report: &mut Report) -> Result<()> {
    crate::remote_display::force_local_for_smoke();
    let owned = Report {
        root: report.root.clone(),
        requested: report.requested.clone(),
        groups: std::mem::take(&mut report.groups),
        stages: std::mem::take(&mut report.stages),
    };
    let holder = Rc::new(RefCell::new(Some(owned)));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("真实UI排空未完成".to_string())));
    let final_error = error.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            if let Err(e) = cx.open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Host),
            ) {
                *error.borrow_mut() = Some(e.to_string());
                cx.quit();
                return;
            }
            cx.spawn(async move |cx| {
                let mut report = holder.borrow_mut().take().expect("独占QA报告");
                *error.borrow_mut() = verify(region, &mut report, cx)
                    .await
                    .err()
                    .map(|e| e.to_string());
                *holder.borrow_mut() = Some(report);
                let _ = cx.update(|cx| {
                    for w in cx.windows() {
                        let _ = cx.update_window(w, |_, w, _| w.remove_window());
                    }
                    cx.quit();
                });
            })
            .detach();
        });
    *report = final_holder.borrow_mut().take().ok_or("真实UI报告丢失")?;
    if let Some(error) = final_error.borrow_mut().take() {
        return Err(error.into());
    }
    Ok(())
}
