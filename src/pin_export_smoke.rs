//! 放大输出专项：合成图片、自有 GPUI 窗口、隔离保存目录，不读写系统剪贴板。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    gpui_capture::hwnd,
    pin_model::PinState,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};
const FOLDER: &str = "target/qa/pin-enlarged-export";
fn ensure(ok: bool, text: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(text.into())
    }
}
fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn fixture() -> Image {
    Image {
        width: 256,
        height: 192,
        bgra: [50, 120, 180, 255].repeat(256 * 192),
        readback_ms: 0.,
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(40))
        .await;
}
fn snapshot(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
) -> Result<crate::pin_export::UserExportSnapshot> {
    pin.update(cx, |v, _, cx| v.user_export_snapshot(cx))
        .map_err(|e| e.to_string())?
}
fn internal(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Image> {
    pin.update(cx, |v, _, cx| v.output(cx))
        .map_err(|e| e.to_string())?
}
fn revision(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<(u64, bool, bool)> {
    pin.update(cx, |v, _, cx| {
        let e = &v.canvas.as_ref().unwrap().read(cx).editor;
        (
            e.document.edit_revision(),
            e.document.can_undo(),
            e.document.can_redo(),
        )
    })
    .map_err(|e| e.to_string().into())
}
async fn verify(
    pin: WindowHandle<PinView>,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..8 {
        tick(cx).await;
    }
    let original = internal(pin, cx)?;
    let baseline = revision(pin, cx)?;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Zoom(1.5), w, cx))
        .map_err(|e| e.to_string())?;
    let snap = snapshot(pin, cx)?;
    ensure(
        (snap.projection.width, snap.projection.height) == (384, 288),
        "150%导出尺寸错误",
    )?;
    let exported = cx
        .background_executor()
        .spawn(async move { snap.render().map_err(|e| e.to_string()) })
        .await
        .map_err(|e| e.to_string())?;
    let expanded = crate::output_projection::OutputProjection::new(Rect::full(256, 192), 1.5)?;
    let nearest = expanded.background(&crate::scene::SourcePixels {
        fill: None,
        canvas: Rect::full(256, 192),
        planes: vec![crate::scene::PixelPlane {
            rect: Rect::full(256, 192),
            bgra: &original.bgra,
        }],
        overlay: None,
    })?;
    let difference = exported
        .bgra
        .iter()
        .zip(&nearest.bgra)
        .filter(|(a, b)| a != b)
        .count();
    ensure(difference > 100, "放大输出仍是整张1×标注最近邻放大")?;
    ensure(
        revision(pin, cx)? == baseline,
        "放大输出改变文档或undo/redo",
    )?;
    let after = internal(pin, cx)?;
    ensure(
        after.width == 256 && after.height == 192 && after.bgra == original.bgra,
        "内部output受用户导出比例影响",
    )?;
    logs.push(serde_json::json!({"group":"native-enlarged-live-vector-source-contract","ok":true,"dimensions":[384,288],"different_channels_from_nearest_composite":difference,"internal_output_exact":true,"undo_redo_unchanged":true}));

    // 保持真实窗口活着，观察后台导出时自有 GPUI事件循环仍在处理请求。
    let output = crate::preferences::data_dir().join("output");
    let previous_files: std::collections::HashSet<_> = std::fs::read_dir(&output)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    pin.update(cx, |v, w, cx| {
        v.state.opacity = 128;
        v.perform(PinAction::QuickSave, w, cx)
    })
    .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut busy_seen = false;
    let mut callbacks = 0;
    loop {
        tick(cx).await;
        callbacks += 1;
        let (busy, status) = pin
            .update(cx, |v, _, _| (v.saving, v.status.clone()))
            .map_err(|e| e.to_string())?;
        busy_seen |= busy;
        if !busy {
            ensure(
                status.starts_with("已保存："),
                &format!("快速保存失败：{status}"),
            )?;
            break;
        }
        ensure(
            started.elapsed() < Duration::from_secs(15),
            "后台快速保存超时",
        )?;
    }
    let path = std::fs::read_dir(&output)?
        .filter_map(|e| e.ok())
        .filter(|e| !previous_files.contains(&e.path()))
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .ok_or("快速保存未生成PNG")?
        .path();
    let saved = crate::export::read_image(&path)?;
    ensure(
        (saved.width, saved.height) == (384, 288),
        "快速保存仍使用原尺寸",
    )?;
    let mut expected = exported;
    crate::output_projection::window_opacity(&mut expected, 128);
    ensure(
        saved.bgra == expected.bgra,
        "快速保存与目标重绘/窗口透明度不一致",
    )?;
    logs.push(serde_json::json!({"group":"native-quick-save-background-export-opacity","ok":true,"png":path,"pixels_exact":true,"callbacks_while_waiting":callbacks,"busy_observed_after_first_tick":busy_seen,"elapsed_ms":started.elapsed().as_secs_f64()*1000.}));

    // 保存按钮的实际入口打开 SavePanel；检查用户可见尺寸命名并用本窗口Esc取消。
    pin.update(cx, |v, w, cx| v.perform(PinAction::Save, w, cx))
        .map_err(|e| e.to_string())?;
    let mut panel = None;
    for _ in 0..80 {
        tick(cx).await;
        panel = cx
            .update(|cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|w| w.downcast::<crate::save_panel::SavePanel>())
            })
            .map_err(|e| e.to_string())?;
        if panel.is_some() {
            break;
        }
    }
    let panel = panel.ok_or("保存入口未打开SavePanel")?;
    let (name, input) = panel
        .update(cx, |v, w, cx| {
            (
                v.filename.read(cx).text().to_owned(),
                crate::gpui_smoke::Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
            )
        })
        .map_err(|e| e.to_string())?;
    ensure(name.contains("384x288"), "保存页文件名仍使用原尺寸")?;
    input.key(windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 as usize);
    for _ in 0..20 {
        tick(cx).await;
        if !pin
            .update(cx, |v, _, _| v.saving)
            .map_err(|e| e.to_string())?
        {
            break;
        }
    }
    ensure(
        !pin.update(cx, |v, _, _| v.saving)
            .map_err(|e| e.to_string())?,
        "取消保存没有解除busy",
    )?;
    logs.push(serde_json::json!({"group":"native-save-panel-target-dimensions-cancel","ok":true,"filename":name,"cancel_native_key":true}));

    pin.update(cx, |v, w, cx| {
        v.state.opacity = 255;
        v.perform(PinAction::Zoom(0.5), w, cx)
    })
    .map_err(|e| e.to_string())?;
    let small = snapshot(pin, cx)?.render()?;
    ensure(
        small.bgra == original.bgra && (small.width, small.height) == (256, 192),
        "缩小贴图导出没有保留原尺寸/1×像素",
    )?;
    // 文件拖出准备使用同一目标图，验证PNG而不启动OLE或移动用户指针。
    let drag = crate::pin_drag::prepare(expected)?;
    let drag_image = crate::export::read_image(&drag)?;
    ensure(
        (drag_image.width, drag_image.height) == (384, 288),
        "拖出文件尺寸错误",
    )?;
    logs.push(serde_json::json!({"group":"native-shrunk-original-drag-file-contract","ok":true,"shrunk_output_exact":true,"drag_png":drag,"ole_drag_started":false}));

    pin.update(cx, |v, _, cx| {
        v.canvas.as_ref().unwrap().update(cx, |c, _| {
            c.editor.select_tool(Tool::Text);
            c.editor.begin_text(None, point(15., 145.));
            c.editor.set_text("首笔 A 汉字".into());
        });
        v.state.zoom = 10_000.;
    })
    .map_err(|e| e.to_string())?;
    let before = revision(pin, cx)?;
    ensure(snapshot(pin, cx).is_err(), "超限导出未拒绝")?;
    ensure(revision(pin, cx)? == before, "超限导出提交或清除文档历史")?;
    ensure(
        pin.update(cx, |v, _, cx| {
            v.canvas
                .as_ref()
                .unwrap()
                .read(cx)
                .editor
                .text_draft
                .is_some()
        })
        .map_err(|e| e.to_string())?,
        "超限导出消耗文字草稿",
    )?;
    pin.update(cx, |v, _, _| v.state.zoom = 2.)
        .map_err(|e| e.to_string())?;
    let with_text = snapshot(pin, cx)?.render()?;
    ensure(
        (with_text.width, with_text.height) == (512, 384),
        "文字草稿导出尺寸错误",
    )?;
    ensure(
        pin.update(cx, |v, _, cx| {
            let e = &v.canvas.as_ref().unwrap().read(cx).editor;
            e.text_draft.is_none()
                && e.document
                    .items()
                    .iter()
                    .any(|o| o.annotation.text == "首笔 A 汉字")
        })
        .map_err(|e| e.to_string())?,
        "有效文字草稿未提交导出",
    )?;
    logs.push(serde_json::json!({"group":"native-budget-rejection-preserves-draft-history","ok":true,"over_limit_not_allocated":true,"draft_preserved_on_failure":true,"valid_export_commits_draft":true}));
    pin.update(cx, |v, _, cx| {
        v.canvas.as_ref().unwrap().update(cx, |c, _| {
            let old = crate::crop_mask::Polygon::new(&[
                point(1., 1.),
                point(7., 1.),
                point(7., 7.),
                point(1., 7.),
            ])
            .unwrap();
            c.editor.document.set_crop_mask(Some(old));
            c.editor.select_tool(Tool::Lasso);
            c.editor.pointer_down((10, 10), 4.);
            c.editor.pointer_move((240, 10));
            c.editor.pointer_move((240, 180));
            c.editor.pointer_move((10, 180));
        });
        v.state.zoom = 100.;
    })
    .map_err(|e| e.to_string())?;
    let lasso_revision = revision(pin, cx)?;
    ensure(
        snapshot(pin, cx).is_err(),
        "预算仍使用旧小套索，遗漏当前大套索",
    )?;
    ensure(
        revision(pin, cx)? == lasso_revision
            && pin
                .update(cx, |v, _, cx| {
                    v.canvas.as_ref().unwrap().read(cx).editor.drawing_lasso()
                })
                .map_err(|e| e.to_string())?,
        "超限结束了套索手势",
    )?;
    pin.update(cx, |v, _, cx| {
        v.state.zoom = 1.;
        v.canvas.as_ref().unwrap().update(cx, |c, _| {
            c.editor.cancel_gesture();
            c.editor.clear_lasso();
        });
    })
    .map_err(|e| e.to_string())?;
    logs.push(serde_json::json!({"group":"native-pending-lasso-budget-before-finish","ok":true,"old_small_mask_not_used":true,"gesture_history_preserved":true}));

    let rotated = cx
        .update(|cx| {
            PinView::open_state(
                Image {
                    width: 64,
                    height: 40,
                    bgra: [10, 20, 30, 128].repeat(64 * 40),
                    readback_ms: 0.,
                },
                (600, 100),
                PinState {
                    zoom: 1.5,
                    opacity: 128,
                    adjustments: crate::pin_model::Adjustments {
                        turns: 1,
                        flip_x: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                cx,
            )
        })
        .map_err(|e| e.to_string())??;
    let rotated_image = snapshot(rotated, cx)?.render()?;
    let rotated_internal = internal(rotated, cx)?;
    ensure(
        (rotated_image.width, rotated_image.height) == (60, 96),
        "已旋转的贴图输出再次交换了宽高",
    )?;
    ensure(
        rotated_image
            .bgra
            .chunks_exact(4)
            .all(|p| p == [10, 20, 30, 64])
            && (rotated_internal.width, rotated_internal.height) == (40, 64)
            && rotated_internal
                .bgra
                .chunks_exact(4)
                .all(|p| p == [10, 20, 30, 128]),
        "源透明度与窗口透明度没有只乘一次",
    )?;
    rotated
        .update(cx, |v, w, cx| {
            v.close_toolbar(cx);
            w.remove_window();
        })
        .map_err(|e| e.to_string())?;
    logs.push(serde_json::json!({"group":"native-rotated-source-alpha-window-opacity-once","ok":true,"dimensions":[60,96],"source_alpha":128,"window_opacity":128,"output_alpha":64,"internal_alpha":128}));
    pin.update(cx, |v, w, cx| {
        v.perform(PinAction::Zoom(1.), w, cx);
        v.set_canvas_active(true, w, cx)
            .expect("导出夹具开启画布成功");
        v.canvas.as_ref().unwrap().update(cx, |c, cx| {
            c.editor.select_tool(Tool::Pen);
            cx.notify();
        });
    })
    .map_err(|e| e.to_string())?;
    for _ in 0..4 {
        tick(cx).await;
    }
    let owned_input = pin
        .update(cx, |_, w, _| crate::gpui_smoke::Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    unsafe {
        use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
        let h = HWND(owned_input.handle as *mut _);
        PostMessageW(Some(h), WM_LBUTTONDOWN, WPARAM(1), LPARAM((30 << 16) | 30))?;
        PostMessageW(
            Some(h),
            WM_MOUSEMOVE,
            WPARAM(crate::gpui_smoke::SMOKE_MOVE | 1),
            LPARAM((60 << 16) | 80),
        )?;
    }
    for _ in 0..4 {
        tick(cx).await;
    }
    ensure(
        pin.update(cx, |v, w, cx| {
            let c = v.canvas.as_ref().unwrap().read(cx);
            c.editor.is_dragging()
                && c.pointer_down_start.is_some()
                && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == hwnd(w) }
        })
        .map_err(|e| e.to_string())?,
        "原生按住左键未建立自有绘制capture",
    )?;
    let held = snapshot(pin, cx)?;
    let held_image = held.render()?;
    ensure(!held_image.bgra.is_empty(), "按住左键导出为空")?;
    ensure(
        pin.update(cx, |v, _, cx| {
            let c = v.canvas.as_ref().unwrap().read(cx);
            !c.editor.is_dragging()
                && c.pointer_down_start.is_none()
                && c.input.drag_anchor.is_none()
                && unsafe {
                    windows::Win32::UI::Input::KeyboardAndMouse::GetCapture()
                        .0
                        .is_null()
                }
        })
        .map_err(|e| e.to_string())?,
        "输出结束后绘制输入追踪或capture未释放",
    )?;
    unsafe {
        use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
        PostMessageW(
            Some(HWND(owned_input.handle as *mut _)),
            WM_LBUTTONUP,
            WPARAM(0),
            LPARAM((60 << 16) | 80),
        )?;
    }
    logs.push(serde_json::json!({"group":"native-held-gesture-output-releases-input-capture","ok":true,"input":"owned PostMessage left-down/move without up before export","native_ctrl_s_tested":false,"capture_released":true,"pointer_tracking_cleared":true}));
    // 32M边界的输出核使用一像素平面+黑色缺屏语义，不额外分配完整源图。
    // 只测background缓冲成本，文字/几何scratch由各自专项验证。
    let (sender, receiver) = std::sync::mpsc::channel();
    let stage = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
    let renderer_stage = stage.clone();
    let stress=cx.background_executor().spawn(async move {
        let result=(||->Result<serde_json::Value>{
            let source=crate::scene::SourcePixels{fill:None,canvas:Rect::full(1000,8000),planes:vec![crate::scene::PixelPlane{rect:Rect::full(1,1),bgra:&[10,20,30,128]}],overlay:None};
            let before=crate::capture::memory_mib();
            let started=Instant::now();let plan=crate::output_projection::OutputProjection::new(source.canvas,2.)?;
            renderer_stage.store(1,std::sync::atomic::Ordering::Release);
            let image=plan.background(&source)?;let allocated=crate::capture::memory_mib();
            renderer_stage.store(2,std::sync::atomic::Ordering::Release);
            ensure((image.width,image.height)==(2000,16000)&&image.bgra.len()==128_000_000,"32M边界输出大小错误")?;
            ensure(image.bgra[..4]==[10,20,30,128]&&image.bgra[image.bgra.len()-4..]==[0,0,0,255],"边界图像采样错误")?;
            let delta=allocated.1-before.1;ensure(delta<160.,"单个背景输出出现额外整图缓冲")?;
            drop(image);let released=crate::capture::memory_mib();
            Ok(serde_json::json!({"group":"native-32m-output-background-budget","ok":true,"pixels":32_000_000,"bytes":128_000_000,"private_before_mib":before.1,"private_allocated_mib":allocated.1,"private_delta_mib":delta,"private_released_mib":released.1,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"scope":"background buffer only; sparse synthetic source"}))
        })().map_err(|e|e.to_string());let _=sender.send(result);
    });
    let started = Instant::now();
    let mut callbacks = 0;
    let mut concurrent_callbacks = 0;
    let mut stress_report = loop {
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
        callbacks += 1;
        let running = stage.load(std::sync::atomic::Ordering::Acquire) == 1;
        pin.update(cx, |_, _, _| ()).map_err(|e| e.to_string())?;
        if running && stage.load(std::sync::atomic::Ordering::Acquire) == 1 {
            concurrent_callbacks += 1;
        }
        match receiver.try_recv() {
            Ok(report) => break report.map_err(|e| e.to_string())?,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ensure(started.elapsed() < Duration::from_secs(30), "32M输出核超时")?
            }
            Err(_) => return Err("32M输出线程没有报告".into()),
        }
    };
    stress.await;
    stress_report["gpui_callbacks_while_waiting"] = callbacks.into();
    stress_report["gpui_callbacks_confirmed_during_render"] = concurrent_callbacks.into();
    logs.push(stress_report);
    // 至少取得一份自有GPUI framebuffer，证明专项确实有渲染中的原生窗口。
    pin.update(cx, |v, w, cx| {
        v.state.zoom = 1.;
        v.state.opacity = 255;
        v.hovered = false;
        v.set_canvas_active(false, w, cx).expect("导出夹具收栏成功");
        w.request_frame_capture([12, 12, 3, 3])
    })
    .map_err(|e| e.to_string())??;
    let mut frame = None;
    for _ in 0..40 {
        tick(cx).await;
        frame = pin
            .update(cx, |_, w, _| w.take_frame_capture())
            .map_err(|e| e.to_string())?;
        if frame.is_some() {
            break;
        }
    }
    let frame = frame
        .ok_or("自有GPUI帧未读回")?
        .map_err(|e| e.to_string())?;
    ensure(frame.len() == 36, "GPUI帧尺寸错误")?;
    ensure(
        frame[16..20]
            .iter()
            .zip([50_u8, 120, 180, 255])
            .all(|(&a, b)| a.abs_diff(b) <= 4),
        "自有GPU底图像素错误",
    )?;
    logs.push(serde_json::json!({"group":"native-owned-gpui-frame","ok":true,"bytes":frame.len(),"backend":"gpu_framebuffer"}));
    Ok(())
}
pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("贴图放大导出专项未完成".into());
    let setup = (|| -> Result<_> {
        std::fs::create_dir_all(FOLDER)?;
        ensure(
            crate::preferences::data_dir()
                == std::env::current_dir()?.join(FOLDER).join("settings"),
            "专项未隔离配置",
        )?;
        let prefs = crate::preferences::Preferences {
            history: false,
            pin_restore: false,
            ..Default::default()
        };
        prefs.save()?;
        let mut store = crate::save_store::SaveStore::load();
        store.config = crate::save_options::SaveConfig::default();
        store.config.directory = crate::preferences::data_dir().join("output");
        store.config.filename_template = "QA_{width}x{height}_{time}".into();
        store.save()?;
        let pin = PinView::open_state(fixture(), (100, 100), PinState::default(), cx)?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            crate::gpui_smoke::isolate_input(hwnd(w))?;
            v.ensure_canvas(w, cx)?;
            v.canvas.as_ref().unwrap().update(cx, |c, cx| {
                let mut line = Annotation::new(
                    Tool::Arrow,
                    point(25.3, 30.7),
                    Style {
                        width: 2.3,
                        opacity: 0.6,
                        ..Default::default()
                    },
                );
                line.update(point(195.4, 94.2));
                c.editor.document.insert(line);
                let mut r = Annotation::new(
                    Tool::Rectangle,
                    point(25.4, 108.3),
                    Style {
                        radius: 11.,
                        width: 2.7,
                        ..Default::default()
                    },
                );
                r.update(point(120.7, 155.6));
                c.editor.document.insert(r.clone());
                c.editor.document.insert(r);
                c.editor.document.undo();
                c.editor.select_tool(Tool::Edit);
                c.pin_editing = false;
                cx.notify();
            });
            Ok(())
        })??;
        Ok(pin)
    })();
    let pin = match setup {
        Ok(p) => p,
        Err(e) => {
            *errors.borrow_mut() = Some(e.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move|cx|{let mut groups=Vec::new();let error=verify(pin,&mut groups,cx).await.err().map(|e|e.to_string());let report=serde_json::json!({"test":"pin-enlarged-export","ok":error.is_none(),"error":error,"groups":groups,"clipboard_read":false,"clipboard_written":false,"desktop_read":false,"global_pointer_moved":false,"source":"owned-synthetic-pin-and-gpui-framebuffer"});let write=std::fs::write(Path::new(FOLDER).join("report.json"),serde_json::to_vec_pretty(&report).unwrap_or_default());*errors.borrow_mut()=error.or_else(||write.err().map(|e|e.to_string()));println!("{report}");let _=pin.update(cx,|v,w,cx|{v.close_toolbar(cx);w.remove_window();});let _=cx.update(|cx|cx.quit());}).detach();
}
