//! 真实贴图 Edit/Crop 移交：这里只验两个入口，不把重叠原图计作文件预览压力。
use super::*;

type PinWindow = WindowHandle<PinView>;

async fn open_pin(fixture: Fixture, cx: &mut AsyncApp) -> Result<PinWindow> {
    let image = cx
        .background_executor()
        .spawn(async move {
            let (width, height) = fixture.dimensions();
            let length = width as usize * height as usize * 4;
            let mut bgra = Vec::new();
            bgra.try_reserve_exact(length).map_err(|e| e.to_string())?;
            for y in 0..height {
                for x in 0..width {
                    bgra.extend_from_slice(&fixture.pixel(x, y));
                }
            }
            Ok::<_, String>(Image {
                width,
                height,
                bgra,
                readback_ms: 0.,
            })
        })
        .await
        .map_err(|e| e.to_string())?;
    let pin = cx.update(|cx| PinView::open_state(image, (100, 100), PinState::default(), cx))??;
    pin.update(cx, |_, w, _| crate::gpui_smoke::isolate_input(hwnd(w)))??;
    wait_pin(pin, cx).await?;
    Ok(pin)
}

async fn wait_pin(pin: PinWindow, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        presented(pin, cx).await?;
        if pin.update(cx, |v, _, _| {
            !v.processing
                && !v.processing_image_unavailable
                && !v.saving
                && !v.workspace_snapshot_busy()
        })? {
            return Ok(());
        }
        ensure(Instant::now() < end, "等待贴图实际处理/保存 ACK 超时")?;
    }
}

async fn enter_editor(pin: PinWindow, action: PinAction, cx: &mut AsyncApp) -> Result<ImageWindow> {
    ensure(
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<CaptureView>().is_none())
        })?,
        "移交前已有图片编辑器窗口",
    )?;
    let end = Instant::now() + WAIT;
    loop {
        let issued = pin.update(cx, |v, w, cx| {
            if v.processing
                || v.processing_image_unavailable
                || v.saving
                || v.workspace_snapshot_busy()
            {
                return false;
            }
            // 保存 ACK 和实际生产 perform 在同一次 UI 更新中，不改 busy 或私有导出逻辑。
            v.perform(action, w, cx);
            true
        })?;
        if issued {
            break;
        }
        ensure(Instant::now() < end, "贴图移交前保存没有完成")?;
        tick(cx).await;
    }
    let window = loop {
        tick(cx).await;
        let editors = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|w| w.downcast::<CaptureView>())
                .collect::<Vec<_>>()
        })?;
        ensure(editors.len() <= 1, "一次贴图移交产生多个图片编辑器")?;
        if let Some(window) = editors.first() {
            break *window;
        }
        ensure(Instant::now() < end, "真实 PinAction 没有打开图片编辑器")?;
    };
    window.update(cx, |_, w, _| crate::gpui_smoke::isolate_input(hwnd(w)))??;
    ready(window, cx).await?;
    Ok(window)
}

async fn close_pin(
    pin: PinWindow,
    original: &Weak<RenderImage>,
    mut previews: Vec<Weak<RenderImage>>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    wait_pin(pin, cx).await?;
    let (main, toolbar) = pin.update(cx, |v, w, cx| {
        if let Some(canvas) = &v.canvas {
            previews.extend(canvas.read(cx).previews.iter().map(Arc::downgrade));
        }
        let toolbar = v
            .toolbar
            .and_then(|bar| bar.update(cx, |_, w, _| hwnd(w).0 as usize).ok());
        let main = hwnd(w).0 as usize;
        v.perform(PinAction::Close, w, cx);
        (main, toolbar)
    })?;
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        let absent = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<PinView>().is_none())
        })?;
        let main_gone = !unsafe { IsWindow(Some(HWND(main as *mut _))) }.as_bool();
        let toolbar_gone =
            toolbar.is_none_or(|h| !unsafe { IsWindow(Some(HWND(h as *mut _))) }.as_bool());
        if absent
            && main_gone
            && toolbar_gone
            && original.upgrade().is_none()
            && previews.iter().all(|w| w.upgrade().is_none())
        {
            break;
        }
        ensure(
            Instant::now() < end,
            "贴图实体/原HWND/原图或观察到的投影尚未释放",
        )?;
    }
    ensure(
        crate::long_capture_runtime::active_workers() == 0
            && crate::long_capture_pipeline::active_capture_threads() == 0,
        "贴图关闭后仍有长截图线程",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    Ok(
        json!({"hwnd":main,"toolbar_hwnd":toolbar,"entity_removed":true,"owned_hwnd_destroyed":true,
        "toolbar_owned_hwnd_destroyed":true,"original_weak_expired":true,"preview_weaks_expired":true,
        "observed_preview_count":previews.len(),"private_bytes_after_500ms":cpu_memory()?["private_bytes"],
        "capture_workers":0,"consumer_sessions":0}),
    )
}

pub(super) async fn run(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let fixture = Fixture {
        alpha: true,
        horizontal: false,
    };
    for (case, action, expected) in [
        ("edit", PinAction::Edit, Tool::Edit),
        ("crop", PinAction::Crop, Tool::Crop),
    ] {
        report.stage("pin-edit-case-begin", json!({"case":case,"source_dimensions":fixture.dimensions(),"input_probe_available":false,
            "pin_output_materialized":true,"pin_editor_handoff_overlap_excluded_from_file_render_gate":true}), false)?;
        let pin = open_pin(fixture, cx).await?;
        let (original, previews, pin_raw_pointer, pin_raw_id) =
            pin.update(cx, |v, _, cx| -> Result<_> {
                let bytes = v.original.as_bytes(0).ok_or("贴图原始帧缺少像素")?;
                fixture.verify(Rect::full(SHORT, LONG), bytes)?;
                let canvas = v
                    .canvas
                    .as_ref()
                    .ok_or("真实32k贴图尚未建立长图显示画布")?
                    .read(cx);
                ensure(
                    canvas.previews.len() == 1 && canvas.editor.annotations().is_empty(),
                    "初始贴图显示预览/文档状态不正确",
                )?;
                let previews = canvas.previews.iter().map(Arc::downgrade).collect();
                Ok((
                    Arc::downgrade(&v.original),
                    previews,
                    bytes.as_ptr() as usize,
                    v.original.id.0,
                ))
            })??;
        let window = enter_editor(pin, action, cx).await?;
        let initial_tool = window.update(cx, |v, _, _| v.editor.tool)?;
        ensure(
            initial_tool == expected,
            "Pin Edit/Crop 移交后的初始工具不正确",
        )?;
        let mut trace = Trace {
            raw: Weak::new(),
            previews: Vec::new(),
            identity: None,
            raw_pointer: 0,
            raw_id: usize::MAX,
        };
        checkpoint(
            report,
            window,
            fixture,
            None,
            &mut trace,
            1,
            &format!("pin-{case}-fit"),
            true,
            cx,
        )
        .await?;
        ensure(
            trace.raw_pointer != pin_raw_pointer && trace.raw_id != pin_raw_id,
            "贴图 output 移交与编辑器原图来源关系不符合实际生产入口",
        )?;
        button(window, "image-actual", cx).await?;
        ensure(
            window.update(cx, |v, _, _| {
                v.image_canvas
                    .as_ref()
                    .is_some_and(|s| (s.viewport.zoom - 1.).abs() < 0.0001)
            })?,
            "贴图移交后的真实100%按钮未生效",
        )?;
        center_at(window, fixture, 8192., cx).await?;
        checkpoint(
            report,
            window,
            fixture,
            None,
            &mut trace,
            1,
            &format!("pin-{case}-actual-pan-8192"),
            false,
            cx,
        )
        .await?;
        let crop = crop_export(window, fixture, cx).await?;
        ensure(
            crop["annotation_count"].as_u64() == Some(0),
            "贴图移交无标注 crop 计数不正确",
        )?;
        ready(window, cx).await?;
        let (factory, _) = source_probe(window, fixture, None, &mut trace, false, cx)?;
        let editor_close = close_image(window, None, &trace, cx).await?;
        // 原贴图仍正常存活，编辑器 crop/export 没有写回或改变共享的原始像素。
        pin.update(cx, |v, _, _| -> Result<()> {
            fixture.verify(
                Rect::full(SHORT, LONG),
                v.original.as_bytes(0).ok_or("编辑器关闭后贴图原帧缺失")?,
            )
        })??;
        let pin_close = close_pin(pin, &original, previews, cx).await?;
        report.stage("pin-edit-case-closed", json!({"case":case,"requested_action":format!("{action:?}"),"initial_tool":format!("{initial_tool:?}"),
            "factory":factory,"crop":crop,"editor_close":editor_close,"pin_close":pin_close,
            "input_probe_available":false,"real_file_decode":false,"pin_output_materialized":true,
            "pin_editor_raw_arc_shared":false,"pin_original_full_bgra_alpha_preserved":true,
            "pin_editor_handoff_overlap_excluded_from_file_render_gate":true}), false)?;
    }
    report.groups.push(json!({"name":"pin-edit-crop-shared-image-preview","ok":true,"rounds":1,"cases":2,
        "source_dimensions":fixture.dimensions(),"real_pin_edit_and_crop":true,"actual_crop_tool_verified":true,
        "real_ui_zoom_pan_crop":true,"full_raw_bgra_alpha_verified":true,"source_identity_preserved":true,"single_active_preview":true,
        "gpu_stable_blocks_per_checkpoint":2,"gpu_all_pixels_verified":false,"raw_preview_weaks_expired":true,"owned_hwnds_destroyed":true,
        "input_probe_available":false,"real_file_decode":false,"pin_output_materialized":true,"pin_editor_raw_arc_shared":false,
        "pin_editor_handoff_overlap_excluded_from_file_render_gate":true,"three_round_close_gate_verified":false,
        "capture_workers":0,"consumer_sessions":0}));
    report.save(None, false)
}
