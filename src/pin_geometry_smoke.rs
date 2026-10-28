//! 同一最终二进制的独占几何复用 / QA 强制借用对照；仅使用自有长图。
use super::{close, ensure, presented, ready, tick, Report, WAIT};
use crate::{
    capture::Image,
    gpui_capture::hwnd,
    process_memory::cpu_memory,
    pin_model::PinState,
    pin_view::{PinAction, PinView},
    pin_workspace_runtime::{snapshot_pixels, SnapshotPixels},
    Result,
};
use gpui::{AsyncApp, WindowHandle};
use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    sync::{mpsc, Arc, Condvar, Mutex, Weak},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::FILETIME,
    System::Threading::{GetCurrentProcess, GetProcessTimes},
    UI::WindowsAndMessaging::IsWindow,
};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 32_000;
const CLOSE_LIMIT: u64 = 16 * 1024 * 1024;

// 大块内部用于独立 framebuffer 小样，最后一列块保留变化的透明度。
pub(super) fn fixture_pixel(x: u32, y: u32) -> [u8; 4] {
    let bx = x / 256;
    let by = y / 512;
    // 64 列 PNG 小专项同样非均色并包含 alpha；不影响 x128/640 的 GPU 稳定块。
    let fine = if x < 64 { x / 4 } else { 0 };
    [
        (32 + (bx * 47 + by * 13 + fine * 7) % 192) as u8,
        (32 + (bx * 31 + by * 17 + fine * 11) % 192) as u8,
        (32 + (bx * 61 + by * 29 + fine * 13) % 192) as u8,
        if (64..768).contains(&x) {
            255
        } else {
            [0, 64, 128, 192, 255][((x + y) % 5) as usize]
        },
    ]
}
fn fixture(width: u32) -> Image {
    let mut bgra = Vec::with_capacity((width * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..width {
            bgra.extend_from_slice(&fixture_pixel(x, y));
        }
    }
    Image {
        width,
        height: HEIGHT,
        bgra,
        readback_ms: 0.,
    }
}

/// Oracle 独立保存预期状态，不调用生产 GeometryPermutation / Adjustments 投影。
#[derive(Clone, Copy, Debug, Default)]
struct Oracle {
    turns: u8,
    flip_x: bool,
    flip_y: bool,
    bright: bool,
}
impl Oracle {
    fn dimensions(self, width: u32) -> (u32, u32) {
        if self.turns.is_multiple_of(2) {
            (width, HEIGHT)
        } else {
            (HEIGHT, width)
        }
    }
    fn source(self, mut x: u32, mut y: u32, width: u32) -> (u32, u32) {
        let (w, h) = self.dimensions(width);
        if self.flip_x {
            x = w - 1 - x;
        }
        if self.flip_y {
            y = h - 1 - y;
        }
        match self.turns {
            1 => (y, HEIGHT - 1 - x),
            2 => (width - 1 - x, HEIGHT - 1 - y),
            3 => (width - 1 - y, x),
            _ => (x, y),
        }
    }
    fn target(self, x: u32, y: u32, width: u32) -> (u32, u32) {
        let (mut tx, mut ty) = match self.turns {
            1 => (HEIGHT - 1 - y, x),
            2 => (width - 1 - x, HEIGHT - 1 - y),
            3 => (y, width - 1 - x),
            _ => (x, y),
        };
        let (w, h) = self.dimensions(width);
        if self.flip_x {
            tx = w - 1 - tx;
        }
        if self.flip_y {
            ty = h - 1 - ty;
        }
        (tx, ty)
    }
    fn pixel(self, x: u32, y: u32, width: u32) -> [u8; 4] {
        let (sx, sy) = self.source(x, y, width);
        let mut pixel = fixture_pixel(sx, sy);
        if self.bright {
            // Brightness(10) 的明确合同是每个颜色通道加 25，alpha 不变。
            for channel in &mut pixel[..3] {
                *channel = channel.saturating_add(25);
            }
        }
        pixel
    }
    fn verify(self, size: (u32, u32), bytes: &[u8], width: u32) -> Result<()> {
        ensure(size == self.dimensions(width), "独立几何 oracle 尺寸不一致")?;
        ensure(
            bytes.len() == size.0 as usize * size.1 as usize * 4,
            "几何输出字节数错误",
        )?;
        for (y, row) in bytes.chunks_exact(size.0 as usize * 4).enumerate() {
            for (x, pixel) in row.chunks_exact(4).enumerate() {
                let expected = self.pixel(x as u32, y as u32, width);
                if pixel != expected {
                    return Err(format!(
                        "几何/颜色/alpha 像素错误 ({x},{y}): {pixel:?}/{expected:?}; {self:?}"
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
}
pub(super) fn process_cpu_ms() -> Result<f64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )?;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Ok((ticks(kernel) + ticks(user)) as f64 / 10_000.)
}

pub(super) async fn open(
    width: u32,
    force: bool,
    cx: &mut AsyncApp,
) -> Result<WindowHandle<PinView>> {
    let pin =
        cx.update(|cx| PinView::open_state(fixture(width), (100, 100), PinState::default(), cx))??;
    pin.update(cx, |v, w, _| -> Result<()> {
        crate::gpui_smoke::isolate_input(hwnd(w))?;
        v.force_borrowed_geometry_for_qa(force);
        Ok(())
    })??;
    ready(pin, cx).await?;
    Ok(pin)
}
pub(super) async fn wait_hwnd_destroyed(handle: usize, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    // GPUI 的实体/像素释放完成后，原生 DestroyWindow 仍由前台 executor 异步执行。
    while unsafe { IsWindow(Some(windows::Win32::Foundation::HWND(handle as *mut _))) }.as_bool() {
        ensure(
            Instant::now() < end,
            &format!("自有 Pin HWND 在截止时间内未销毁：{handle}"),
        )?;
        tick(cx).await;
    }
    Ok(())
}
fn verify_pixels(
    pin: WindowHandle<PinView>,
    oracle: Oracle,
    width: u32,
    cx: &mut AsyncApp,
) -> Result<()> {
    pin.update(cx, |v, _, _| -> Result<()> {
        ensure(
            !v.processing && !v.processing_image_unavailable,
            "像素检查发生于未完成处理",
        )?;
        let actual = v.state.adjustments;
        ensure(
            (
                actual.turns,
                actual.flip_x,
                actual.flip_y,
                actual.brightness,
            ) == (
                oracle.turns,
                oracle.flip_x,
                oracle.flip_y,
                if oracle.bright { 10 } else { 0 },
            ),
            "真实参数未达到独立预期状态",
        )?;
        oracle.verify(
            v.image_size,
            v.image.as_bytes(0).ok_or("当前帧缺少像素")?,
            width,
        )?;
        Oracle::default().verify(
            v.original_size,
            v.original.as_bytes(0).ok_or("原帧缺少像素")?,
            width,
        )
    })?
}
pub(super) struct Begun {
    pub(super) old: Weak<gpui::RenderImage>,
    pub(super) old_id: usize,
    pub(super) original_id: usize,
    pub(super) started: Instant,
    pub(super) cpu_start: f64,
}
pub(super) async fn perform_after_save(
    pin: WindowHandle<PinView>,
    action: PinAction,
    cx: &mut AsyncApp,
) -> Result<Begun> {
    let end = Instant::now() + WAIT;
    loop {
        let begun = pin.update(cx, |v, w, cx| -> Result<Option<Begun>> {
            // 真实保存完成 ACK 与下一命令在同一 UI 更新中；不篡改 busy 或暂停 tick。
            if v.workspace_snapshot_busy() {
                return Ok(None);
            }
            let begun = Begun {
                old: Arc::downgrade(&v.image),
                old_id: v.image.id.0,
                original_id: v.original.id.0,
                started: Instant::now(),
                cpu_start: process_cpu_ms()?,
            };
            v.perform(action, w, cx);
            Ok(Some(begun))
        })??;
        if let Some(begun) = begun {
            return Ok(begun);
        }
        ensure(Instant::now() < end, "等待实际 workspace 保存 ACK 超时")?;
        tick(cx).await;
    }
}

/// 只读自有 Pin 的 3×3 framebuffer；全图正确性另由借用 CPU 行 oracle 验证。
async fn framebuffer(
    pin: WindowHandle<PinView>,
    oracle: Oracle,
    cx: &mut AsyncApp,
) -> Result<Vec<serde_json::Value>> {
    let (size, client, placement) = pin.update(cx, |v, w, _| -> Result<_> {
        let viewport = w.viewport_size();
        let dpi = w.scale_factor();
        Ok((
            v.image_size,
            (
                (f32::from(viewport.width) * dpi).round() as u32,
                (f32::from(viewport.height) * dpi).round() as u32,
            ),
            crate::pin_native::rect(hwnd(w))?,
        ))
    })??;
    let work = crate::pin_native::work_area((placement.x, placement.y));
    ensure(
        client.0 > 0 && client.1 > 0 && size.0 > 0 && size.1 > 0,
        "GPU 投影尺寸为零",
    )?;
    let zoom =
        (f64::from(client.0) / f64::from(size.0)).min(f64::from(client.1) / f64::from(size.1));
    let offset = (
        (f64::from(client.0) - f64::from(size.0) * zoom) / 2.,
        (f64::from(client.1) - f64::from(size.1) * zoom) / 2.,
    );
    let mut samples = Vec::new();
    for source_x in [128, 640] {
        let source_y = 31 * 512 + 256;
        let (tx, ty) = oracle.target(source_x, source_y, WIDTH);
        let px = (offset.0 + (f64::from(tx) + 0.5) * zoom).floor() as u32;
        let py = (offset.1 + (f64::from(ty) + 0.5) * zoom).floor() as u32;
        let context = format!("oracle={oracle:?}, image={size:?}, client={client:?}, placement={placement:?}, work={work:?}, zoom={zoom}, offset={offset:?}, source=({source_x},{source_y}), output=({tx},{ty}), physical=({px},{py})");
        ensure(
            px >= 4 && py >= 4 && px + 4 < client.0 && py + 4 < client.1,
            &format!("GPU 稳定块中心太靠近客户区边界；{context}"),
        )?;
        let rect = [px - 1, py - 1, 3, 3];
        ensure(
            i64::from(placement.x) + i64::from(rect[0]) >= i64::from(work.x)
                && i64::from(placement.y) + i64::from(rect[1]) >= i64::from(work.y)
                && i64::from(placement.x) + i64::from(rect[0] + 3)
                    <= i64::from(work.x) + i64::from(work.width)
                && i64::from(placement.y) + i64::from(rect[1] + 3)
                    <= i64::from(work.y) + i64::from(work.height),
            &format!("自有 framebuffer 样本不在可见工作区；{context}"),
        )?;
        let expected = oracle.pixel(tx, ty, WIDTH);
        ensure(expected[3] == 255, "GPU 小样源不是不透明块")?;
        // 连续覆盖区间加两源像素的过滤裕量；每个参与颜色块必须相同。
        let bounds = [
            ((f64::from(rect[0]) - offset.0) / zoom).floor() - 2.,
            ((f64::from(rect[1]) - offset.1) / zoom).floor() - 2.,
            ((f64::from(rect[0] + 3) - offset.0) / zoom).ceil() + 2.,
            ((f64::from(rect[1] + 3) - offset.1) / zoom).ceil() + 2.,
        ];
        ensure(
            bounds[0] >= 0.
                && bounds[1] >= 0.
                && bounds[2] <= f64::from(size.0)
                && bounds[3] <= f64::from(size.1),
            &format!("GPU 完整过滤裕量越界；bounds={bounds:?}；{context}"),
        )?;
        let [x0, y0, x1, y1] = bounds.map(|value| value as u32);
        ensure(x0 < x1 && y0 < y1, "GPU 样本覆盖范围为空")?;
        for y in y0..y1 {
            for x in x0..x1 {
                if oracle.pixel(x, y, WIDTH) != expected {
                    return Err(format!("GPU 样本跨过颜色块/alpha 边界；point=({x},{y}), footprint={bounds:?}；{context}").into());
                }
            }
        }
        pin.update(cx, |_, w, _| w.request_frame_capture(rect))??;
        let end = Instant::now() + WAIT;
        let bytes = loop {
            tick(cx).await;
            if let Some(result) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
                break result.map_err(|error| error.to_string())?;
            }
            ensure(Instant::now() < end, "几何自有 framebuffer 读回超时")?;
        };
        ensure(bytes.len() == 36, "GPU 小样字节数错误")?;
        let difference = bytes
            .chunks_exact(4)
            .flat_map(|pixel| pixel.iter().zip(expected).map(|(&a, b)| a.abs_diff(b)))
            .max()
            .unwrap_or(0);
        ensure(difference <= 1, &format!("GPU 几何稳定块错位或旧缓存：{difference}; actual={bytes:?}, expected={expected:?}; {context}"))?;
        samples.push(serde_json::json!({"source_anchor":[source_x,source_y],"output_anchor":[tx,ty],"rect":rect,
            "expected_bgra":expected,"actual_bgra":bytes,"max_difference":difference,"tolerance":1,
            "source":"owned-pin-framebuffer","source_footprint":[x0,y0,x1,y1]}));
    }
    Ok(samples)
}

#[derive(Default)]
struct PngGate {
    entered: bool,
    released: bool,
}
struct HeaderWriter {
    file: File,
    written: usize,
    gate: Arc<(Mutex<PngGate>, Condvar)>,
}
impl Write for HeaderWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written >= 33 {
            let (lock, signal) = &*self.gate;
            let mut state = lock
                .lock()
                .map_err(|_| std::io::Error::other("PNG gate poisoned"))?;
            state.entered = true;
            signal.notify_all();
            while !state.released {
                state = signal
                    .wait(state)
                    .map_err(|_| std::io::Error::other("PNG gate poisoned"))?;
            }
        }
        let count = self.file.write(bytes)?;
        self.written += count;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
pub(super) struct EncodingJob {
    gate: Arc<(Mutex<PngGate>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    completion: mpsc::Receiver<std::result::Result<(), String>>,
}
impl EncodingJob {
    pub(super) fn start(snapshot: SnapshotPixels, path: PathBuf) -> Self {
        let gate = Arc::new((Mutex::new(PngGate::default()), Condvar::new()));
        let writer_gate = gate.clone();
        let (send, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let ((w, h), pixels) = snapshot.pixels()?;
                crate::export::write_png_pixels(
                    w,
                    h,
                    pixels,
                    HeaderWriter {
                        file: File::create(path)?,
                        written: 0,
                        gate: writer_gate,
                    },
                    png::Compression::Fast,
                )
            })()
            .map_err(|error| error.to_string());
            drop(snapshot);
            let _ = send.send(result);
        });
        Self {
            gate,
            worker: Some(worker),
            completion,
        }
    }
    pub(super) fn entered(&self) -> Result<bool> {
        Ok(self.gate.0.lock().map_err(|_| "PNG gate poisoned")?.entered)
    }
    fn release(&self) {
        if let Ok(mut state) = self.gate.0.lock() {
            state.released = true;
            self.gate.1.notify_all();
        }
    }
    pub(super) async fn finish(&mut self, cx: &mut AsyncApp) -> Result<()> {
        self.release();
        let end = Instant::now() + WAIT;
        let result = loop {
            match self.completion.try_recv() {
                Ok(result) => break result,
                Err(mpsc::TryRecvError::Disconnected) => return Err("PNG worker 未返回结果".into()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            ensure(Instant::now() < end, "PNG 保存任务未完成")?;
            tick(cx).await;
        };
        self.worker
            .take()
            .ok_or("PNG worker 缺失")?
            .join()
            .map_err(|_| "PNG worker panic")?;
        result.map_err(Into::into)
    }
}
impl Drop for EncodingJob {
    fn drop(&mut self) {
        // 所有错误出口先解除真实 writer 阻塞；小 PNG 任务仍正常 join，不留挂起线程。
        self.release();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub(super) async fn verify_shared(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let pin = open(64, false, cx).await?;
    let original = pin.update(cx, |v, _, _| Arc::downgrade(&v.original))?;
    let _ = perform_after_save(pin, PinAction::Brightness(10), cx).await?;
    ready(pin, cx).await?;
    let bright = Oracle {
        bright: true,
        ..Default::default()
    };
    verify_pixels(pin, bright, 64, cx)?;
    let (snapshot, old, pointer, image_id, handle) = pin.update(cx, |v, w, _| -> Result<_> {
        let snapshot = snapshot_pixels(&v.image, v.image_size, true, || {
            Err("Shared factory 不应调用输出回退".into())
        })?;
        ensure(
            matches!(&snapshot, SnapshotPixels::Shared { .. }),
            "生产快照工厂未返回 Shared",
        )?;
        let ((width, height), pixels) = snapshot.pixels()?;
        ensure(
            (width, height) == v.image_size
                && pixels.as_ptr() == v.image.as_bytes(0).ok_or("缺少旧图")?.as_ptr(),
            "快照没有借用当前图像",
        )?;
        Ok((
            snapshot,
            Arc::downgrade(&v.image),
            v.image.as_bytes(0).ok_or("缺少旧图")?.as_ptr() as usize,
            v.image.id.0,
            hwnd(w).0 as usize,
        ))
    })??;
    let path = report.root.join("geometry-shared-inflight.png");
    let mut job = EncodingJob::start(snapshot, path.clone());
    let end = Instant::now() + WAIT;
    while !job.entered()? {
        ensure(Instant::now() < end, "真实 PNG 编码器未到 header 后阻塞点")?;
        tick(cx).await;
    }
    let mut header = [0; 33];
    File::open(&path)?.read_exact(&mut header)?;
    ensure(
        &header[..8] == b"\x89PNG\r\n\x1a\n" && &header[12..16] == b"IHDR",
        "PNG 阻塞前没有实际写出 header",
    )?;
    let _ = perform_after_save(pin, PinAction::FlipX, cx).await?;
    ready(pin, cx).await?;
    verify_pixels(
        pin,
        Oracle {
            flip_x: true,
            ..bright
        },
        64,
        cx,
    )?;
    let probe = pin.update(cx, |v, _, _| v.processing_probe())?;
    ensure(
        probe.completed
            && !probe.failed
            && !probe.reused
            && probe.fallback_reason == Some("shared-image")
            && probe.choice == Some("borrowed-original")
            && probe.old_cpu_pointer == pointer
            && probe.old_image_id == image_id
            && probe.new_cpu_pointer != pointer
            && probe.new_image_id != image_id,
        "在途 Shared PNG 未触发安全共享回退",
    )?;
    {
        let saved = old.upgrade().ok_or("编码器阻塞期间丢失 Shared 图像")?;
        bright.verify(
            (64, HEIGHT),
            saved.as_bytes(0).ok_or("在途快照像素缺失")?,
            64,
        )?;
    }
    job.finish(cx).await?;
    ensure(old.upgrade().is_none(), "保存 join 后旧 Shared 快照仍保留")?;
    let decoded = cx
        .background_executor()
        .spawn(async move { crate::export::read_image(&path).map_err(|error| error.to_string()) })
        .await
        .map_err(|error| error.to_string())?;
    bright.verify((decoded.width, decoded.height), &decoded.bgra, 64)?;
    drop(decoded);
    close(pin, original, cx).await?;
    wait_hwnd_destroyed(handle, cx).await?;
    ensure(
        crate::long_capture_runtime::active_workers() == 0
            && crate::long_capture_pipeline::active_capture_threads() == 0,
        "共享 PNG 小专项关闭后仍有采集/消费者线程",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    report.record("geometry-shared-png-inflight", serde_json::json!({
        "source_dimensions":[64,HEIGHT],"raw_bytes":64_u64* u64::from(HEIGHT)*4,
        "real_snapshot_factory_shared":true,"real_streaming_encoder_header_before_flip":true,
        "blocked_old_snapshot_all_pixels_unchanged":true,"decoded_png_all_pixels_correct":true,
        "encoder_thread_joined":true,"snapshot_weak_expired":true,"owned_hwnd_destroyed":true,"probe":probe,
        "workspace_tick_and_manifest_commit_tested":false,"decode_released_before_960px_rounds":true,
    }))?;
    report.groups.push(serde_json::json!({
        "name":"shared-png-pin-geometry-buffer","ok":true,"source_dimensions":[64,HEIGHT],"processed_frames":2,
        "real_snapshot_factory_shared":true,"real_streaming_encoder":true,"shared_fallback_checked":true,
        "old_snapshot_all_bgra_unchanged":true,"new_image_all_bgra_correct":true,"decoded_png_all_bgra_correct":true,
        "encoder_thread_joined":true,"snapshot_weak_expired":true,"source_weak_expired":true,
        "owned_hwnd_destroyed":true,"entity_removed":true,"capture_workers":0,"consumer_sessions":0,
        "workspace_tick_and_manifest_commit_tested":false,"clipboard_untouched":true,"desktop_pixels_read":false,
        "limitations":["生产共享快照工厂及流式编码器直接调用，未覆盖真实工作区tick或manifest提交", "64×32000有限共享保护检查，不是960×32000三轮性能对照"],
    }));
    report.save(None, false)
}

pub(super) async fn verify(
    report: &mut Report,
    cx: &mut AsyncApp,
    force_borrowed: bool,
) -> Result<()> {
    let mut closed = Vec::new();
    let mut reuse_count = 0;
    let mut forced_count = 0;
    let mut shared_count = 0;
    let mut eligible_count = 0;
    let mut unique_by_kind = std::collections::BTreeMap::<&str, u32>::new();
    for round in 1..=3 {
        let pin = open(WIDTH, force_borrowed, cx).await?;
        let (original, handle) = pin.update(cx, |v, w, _| {
            (Arc::downgrade(&v.original), hwnd(w).0 as usize)
        })?;
        verify_pixels(pin, Oracle::default(), WIDTH, cx)?;
        let bright = Oracle {
            bright: true,
            ..Default::default()
        };
        let flipped = Oracle {
            flip_x: true,
            ..bright
        };
        let half = Oracle {
            turns: 2,
            ..flipped
        };
        let both = Oracle {
            flip_y: true,
            ..half
        };
        let quarter = Oracle {
            turns: 1,
            ..Default::default()
        };
        let steps = [
            ("brightness", PinAction::Brightness(10), bright, false),
            ("flip-x", PinAction::FlipX, flipped, true),
            ("rotate-half", PinAction::Rotate(2), half, true),
            ("flip-y", PinAction::FlipY, both, true),
            ("undo", PinAction::Undo, half, true),
            ("redo", PinAction::Redo, both, true),
            ("reset", PinAction::ResetImage, Oracle::default(), false),
            (
                "rotate-quarter-fallback",
                PinAction::Rotate(1),
                quarter,
                false,
            ),
            (
                "flip-after-quarter",
                PinAction::FlipX,
                Oracle {
                    flip_x: true,
                    ..quarter
                },
                true,
            ),
        ];
        let mut life = pin.update(cx, |v, _, _| v.preview_texture_lifecycle())?;
        let mut previous = Oracle::default();
        let mut old_frames = Vec::new();
        let mut round_reuses = 0;
        for (step, action, oracle, eligible) in steps {
            let Begun {
                old,
                old_id,
                original_id,
                started,
                cpu_start,
            } = perform_after_save(pin, action, cx).await?;
            ready(pin, cx).await?;
            presented(pin, cx).await?;
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.;
            let cpu_ms = process_cpu_ms()? - cpu_start;
            verify_pixels(pin, oracle, WIDTH, cx)?;
            let samples = framebuffer(pin, oracle, cx).await?;
            let (probe, next_life, current_id, current_pointer) = pin.update(cx, |v, _, _| {
                (
                    v.processing_probe(),
                    v.preview_texture_lifecycle(),
                    v.image.id.0,
                    v.image
                        .as_bytes(0)
                        .map_or(0, |bytes| bytes.as_ptr() as usize),
                )
            })?;
            ensure(
                probe.completed && !probe.failed && !probe.image_unavailable,
                "处理 probe 尚未成功结束",
            )?;
            ensure(
                current_id != old_id && next_life.1 == 0 && next_life.3 > life.3,
                "处理没有新 ImageId / 实际旧预览退休",
            )?;
            if step == "reset" {
                ensure(
                    probe.choice == Some("reset-original") && current_id == original_id,
                    "Reset 没有回到原图",
                )?;
            } else {
                ensure(
                    probe.new_image_id == current_id
                        && probe.new_cpu_pointer == current_pointer
                        && probe.old_image_id == old_id
                        && probe.old_byte_length == (WIDTH * HEIGHT * 4) as usize
                        && probe.new_byte_length == probe.old_byte_length,
                    "数字 probe 未绑定真实新旧图像",
                )?;
                if !probe.reused {
                    ensure(
                        probe.old_cpu_pointer != probe.new_cpu_pointer,
                        "实际借用重建路径意外报告同一个旧 buffer",
                    )?;
                }
                if eligible {
                    eligible_count += 1;
                }
                if eligible && !force_borrowed && probe.reused {
                    ensure(
                        probe.reused
                            && probe.choice == Some("unique-buffer")
                            && probe.fallback_reason.is_none()
                            && probe.old_cpu_pointer == probe.new_cpu_pointer,
                        "可独占几何操作没有真实 try_unwrap 命中",
                    )?;
                    reuse_count += 1;
                    round_reuses += 1;
                    *unique_by_kind.entry(step).or_default() += 1;
                } else if eligible && !force_borrowed {
                    ensure(
                        !probe.reused
                            && probe.choice == Some("borrowed-original")
                            && probe.fallback_reason == Some("shared-image"),
                        "实际未复用的几何路径没有合法 Shared 原因证据",
                    )?;
                    shared_count += 1;
                } else if eligible && force_borrowed {
                    ensure(
                        !probe.reused
                            && probe.fallback_reason == Some("qa-forced-borrowed")
                            && probe.choice == Some("borrowed-original"),
                        "A/B 借用组没有实际 QAForced 选择",
                    )?;
                    forced_count += 1;
                } else {
                    ensure(
                        !probe.reused
                            && probe.choice == Some("borrowed-original")
                            && probe.fallback_reason == Some("original-image"),
                        "颜色/quarter 操作未回退借用原图",
                    )?;
                }
            }
            let old_expired = old.upgrade().is_none();
            let shared_old_all_pixels_checked = if probe.fallback_reason == Some("shared-image") {
                if let Some(saved) = old.upgrade() {
                    previous.verify(
                        previous.dimensions(WIDTH),
                        saved.as_bytes(0).ok_or("旧 Shared 帧缺少像素")?,
                        WIDTH,
                    )?;
                    Some(true)
                } else {
                    None
                }
            } else {
                None
            };
            if probe.reused {
                ensure(old_expired, "移动 buffer 后旧 Arc 没有消失")?;
            }
            report.record("geometry-processed-presented", serde_json::json!({
                "round":round,"step":step,"force_borrowed":force_borrowed,"width":oracle.dimensions(WIDTH).0,
                "height":oracle.dimensions(WIDTH).1,"all_pixels_checked":u64::from(WIDTH)*u64::from(HEIGHT),
                "all_alpha_checked":true,"original_all_pixels_unchanged":true,"expected_image_materialized":false,
                "workspace_save_busy_false_at_atomic_perform":true,
                "processing_through_presentation_wall_ms":elapsed_ms,"process_cpu_ms":cpu_ms,
                "probe":probe,"lifecycle":next_life,"framebuffer_samples":samples,"old_weak_expired":old_expired,
                "shared_old_all_pixels_checked_while_owner_alive":shared_old_all_pixels_checked,
                "shared_owner_identity_inspected":false,
            }))?;
            life = next_life;
            previous = oracle;
            old_frames.push(old);
        }
        let current = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
        close(pin, original, cx).await?;
        wait_hwnd_destroyed(handle, cx).await?;
        ensure(current.upgrade().is_none(), "关闭后当前处理图仍保留")?;
        ensure(
            old_frames.iter().all(|old| old.upgrade().is_none()),
            "关闭后某个旧几何帧仍被持有",
        )?;
        ensure(
            crate::long_capture_runtime::active_workers() == 0
                && crate::long_capture_pipeline::active_capture_threads() == 0,
            "几何专项关闭后存在采集/消费者线程",
        )?;
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        let memory = cpu_memory()?;
        let private = memory["private_bytes"]
            .as_u64()
            .ok_or("关闭内存缺少 private_bytes")?;
        closed.push(private);
        report.record("geometry-round-closed", serde_json::json!({
            "round":round,"private_bytes_after_500ms":private,"source_weak_expired":true,"current_weak_expired":true,
            "entity_removed":true,"owned_hwnd_destroyed":true,"capture_workers":0,"consumer_sessions":0,
            "actual_unique_buffer_hits":round_reuses,"old_frame_weaks_expired":old_frames.len(),
        }))?;
        ensure(
            force_borrowed || round_reuses >= 3,
            "本轮实际 unique 命中少于三次，仍不能完成复用验收",
        )?;
    }
    let growth = closed
        .iter()
        .copied()
        .max()
        .ok_or("没有关闭内存样本")?
        .saturating_sub(closed[0]);
    report.record("geometry-close-memory-gate", serde_json::json!({"private_bytes":closed,"growth_bytes":growth,"limit_bytes":CLOSE_LIMIT,"ok":growth<=CLOSE_LIMIT}))?;
    ensure(
        growth <= CLOSE_LIMIT,
        "三轮几何贴图关闭内存增长超过原 16MiB 门",
    )?;
    ensure(eligible_count == 18, "A/B eligible 路径数量错误")?;
    if force_borrowed {
        ensure(
            forced_count == 18 && reuse_count == 0,
            "借用 A/B 实际 QAForced 选择次数不足",
        )?;
    } else {
        ensure(
            [
                "flip-x",
                "rotate-half",
                "flip-y",
                "undo",
                "redo",
                "flip-after-quarter",
            ]
            .iter()
            .all(|kind| unique_by_kind.get(kind).is_some_and(|count| *count >= 1)),
            "至少一种几何操作仍没有实际 unique 命中，changed path 原生覆盖未完成",
        )?;
    }
    report.groups.push(serde_json::json!({
        "name":if force_borrowed {"three-rounds-32000px-pin-geometry-borrowed"} else {"three-rounds-32000px-pin-geometry-reuse"},
        "ok":true,"rounds":3,"cycles":3,"steps_per_round":9,"processed_frames":27,"unique_buffer_hits":reuse_count,"qa_forced_borrowed_hits":forced_count,
        "eligible_geometry_steps":eligible_count,"shared_fallback_hits":shared_count,"unique_by_kind":unique_by_kind,
        "source_dimensions":[WIDTH,HEIGHT],"shared_png_inflight_checked":false,
        "close_private_bytes":closed,"close_private_growth_bytes":growth,"close_private_limit_bytes":CLOSE_LIMIT,
        "source":"owned-synthetic-pin-with-independent-inverse-geometry-oracle","same_binary_ab":true,
        "all_bgra_pixels_checked":true,"gpu_all_pixels_checked":false,"gpu_sampled_stable_blocks_per_step":2,
        "clipboard_untouched":true,"desktop_pixels_read":false,"global_input_sent":false,
        "limitations":["借用组是相同最终二进制的 QA 强制路径，不是旧版本二进制测量", "GPU 仅独立不透明稳定块小样，全部 BGRA/alpha 由 CPU 行 oracle 检查", "共享PNG保护另设独立worker，不预热本组", "有限三轮不证明长期无泄漏或真实浏览器/采集硬件表现"],
    }));
    report.save(None, false)
}
