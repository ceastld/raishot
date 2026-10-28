//! 真实工作区静置合并、失败重试和关闭；不在快速操作间运行全图 oracle。
use super::{ensure, geometry, presented, ready, tick, Report, WAIT};
use crate::{
    gpui_capture::hwnd,
    process_memory::cpu_memory,
    pin_model::Adjustments,
    pin_view::{PinAction, PinView},
    pin_workspace::{self as store, PinEntry, PinStatus},
    pin_workspace_runtime::SnapshotPause,
    Result,
};
use gpui::{AsyncApp, WeakEntity, WindowHandle};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_CLOSE},
};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 32_000;
const PIXELS: u64 = WIDTH as u64 * HEIGHT as u64;
const QUIET_MS: u64 = 750;
const MAX_DELAY_MS: u64 = 3_000;
const CLOSE_LIMIT: u64 = 16 * 1024 * 1024;

/// 独立字面量公式；不调用生产几何、重填或颜色投影。
#[derive(Clone, Copy, Debug, Default)]
struct Oracle {
    brightness: i16,
    flip_x: bool,
}
impl Oracle {
    fn pixel(self, x: u32, y: u32) -> [u8; 4] {
        let sx = if self.flip_x { WIDTH - 1 - x } else { x };
        let mut p = geometry::fixture_pixel(sx, y);
        for channel in &mut p[..3] {
            *channel = (i16::from(*channel) + self.brightness * 255 / 100).clamp(0, 255) as u8;
        }
        p
    }
    fn verify(self, size: (u32, u32), bytes: &[u8]) -> Result<()> {
        ensure(size == (WIDTH, HEIGHT), "静置专项图像尺寸错误")?;
        ensure(bytes.len() == PIXELS as usize * 4, "静置专项像素长度错误")?;
        for (y, row) in bytes.chunks_exact(WIDTH as usize * 4).enumerate() {
            for (x, p) in row.chunks_exact(4).enumerate() {
                let expected = self.pixel(x as u32, y as u32);
                if p != expected {
                    return Err(format!(
                        "静置专项 BGRA/alpha 错误 ({x},{y}): {p:?}/{expected:?}; {self:?}"
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
}
fn bright(brightness: i16, flip_x: bool) -> Oracle {
    Oracle { brightness, flip_x }
}
fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
fn count(probe: &Value, field: &str) -> Result<u64> {
    probe[field]
        .as_u64()
        .ok_or_else(|| format!("workspace probe 缺少 {field}").into())
}
fn pending(probe: &Value) -> bool {
    !matches!(
        probe.get("pending_close"),
        None | Some(Value::Null) | Some(Value::Bool(false))
    )
}
fn busy(probe: &Value) -> bool {
    probe["busy"].as_bool() == Some(true)
}
fn probe(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Value> {
    let value = pin.update(cx, |v, _, _| -> Result<Value> {
        Ok(serde_json::to_value(
            v.workspace_snapshot_probe()
                .ok_or("静置专项缺少工作区 probe")?,
        )?)
    })??;
    ensure(
        count(&value, "quiet_ms")? == QUIET_MS && count(&value, "max_delay_ms")? == MAX_DELAY_MS,
        "实际静置策略参数不符",
    )?;
    Ok(value)
}
fn entry(id: &str) -> Result<PinEntry> {
    store::load()?
        .pins
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| "静置专项工作区记录丢失".into())
}
fn asset(id: &str) -> Result<PathBuf> {
    store::asset_path(
        entry(id)?
            .image_file
            .as_deref()
            .ok_or("静置专项快照未提交")?,
    )
}
fn verify_png(path: &Path, oracle: Oracle) -> Result<()> {
    let mut reader = png::Decoder::new(BufReader::new(File::open(path)?)).read_info()?;
    let info = reader.info();
    ensure(
        (info.width, info.height) == (WIDTH, HEIGHT)
            && info.color_type == png::ColorType::Rgba
            && info.bit_depth == png::BitDepth::Eight,
        "静置 PNG 格式或尺寸错误",
    )?;
    let mut y = 0;
    while let Some(row) = reader.next_row()? {
        ensure(row.data().len() == WIDTH as usize * 4, "静置 PNG 行宽错误")?;
        for (x, rgba) in row.data().chunks_exact(4).enumerate() {
            let p = oracle.pixel(x as u32, y);
            if rgba != [p[2], p[1], p[0], p[3]] {
                return Err(format!("静置 PNG 全像素/alpha 错误 ({x},{y}); {oracle:?}").into());
            }
        }
        y += 1;
    }
    ensure(y == HEIGHT, "静置 PNG 行数不足")
}
/// 归档真实已提交的压缩 PNG；不解码成第二幅完整图。
fn archive(report: &Report, name: &str, id: &str, oracle: Oracle) -> Result<Value> {
    let source = asset(id)?;
    let directory = report.root.join("owned-assets");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    ensure(!path.exists(), "静置归档不得覆盖已有证据")?;
    let bytes = std::fs::copy(&source, &path)?;
    Ok(
        json!({"path":path,"bytes":bytes,"actual_source_path":source,"entry_id":id,"width":WIDTH,"height":HEIGHT,"expected_adjustments":{"brightness":oracle.brightness,"flip_x":oracle.flip_x,"flip_y":false,"turns":0,"grayscale":false,"invert":false},"annotation_count":0,"source":"actual-committed-workspace-png","copied_without_full_decode":true}),
    )
}
fn verify_pixels(pin: WindowHandle<PinView>, expected: Oracle, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, _, _| -> Result<()> {
        ensure(
            !v.processing && !v.processing_image_unavailable,
            "全图验证时处理尚未完成",
        )?;
        let a = v.state.adjustments;
        ensure(
            a.turns == 0
                && a.flip_x == expected.flip_x
                && !a.flip_y
                && a.brightness == expected.brightness
                && !a.grayscale
                && !a.invert,
            "静置专项实际参数错误",
        )?;
        expected.verify(v.image_size, v.image.as_bytes(0).ok_or("当前图没有像素")?)?;
        Oracle::default().verify(
            v.original_size,
            v.original.as_bytes(0).ok_or("原图没有像素")?,
        )
    })?
}

/// 保存屏障在退出或失败时总是释放；只延长编码/提交，不阻止真实 tick 启动。
struct Pause(Arc<SnapshotPause>);
impl Pause {
    fn new(before_encode: bool) -> Self {
        Self(SnapshotPause::new(before_encode))
    }
    fn install(&self, pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
        pin.update(cx, |v, _, _| {
            v.pause_next_workspace_snapshot_for_qa(self.0.clone())
        })?
    }
    fn release_encode(&self) {
        self.0.release_encode();
    }
    fn release_commit(&self) {
        self.0.release_commit();
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.0.release_encode();
        self.0.release_commit();
    }
}
async fn stage(
    pause: &Pause,
    expected: u8,
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let end = Instant::now() + WAIT;
    while pause.0.stage() != expected {
        ensure(Instant::now() < end, "静置保存未进入实际编码阶段")?;
        tick(cx).await;
    }
    let p = probe(pin, cx)?;
    ensure(busy(&p), "真实编码阶段没有 busy")?;
    Ok(p)
}
async fn saved(
    pin: WindowHandle<PinView>,
    id: &str,
    previous: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let end = Instant::now() + WAIT;
    loop {
        let p = probe(pin, cx)?;
        if !busy(&p)
            && entry(id)?
                .image_file
                .as_deref()
                .is_some_and(|v| Some(v) != previous)
        {
            return Ok(p);
        }
        ensure(Instant::now() < end, "静置 PNG/清单提交 ACK 超时")?;
        tick(cx).await;
    }
}
async fn processed(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let complete = pin.update(cx, |v, _, _| -> Result<bool> {
            if v.processing {
                return Ok(false);
            }
            let p = v.processing_probe();
            ensure(
                p.completed && !p.failed && !v.processing_image_unavailable,
                "实际贴图处理失败",
            )?;
            Ok(true)
        })??;
        if complete {
            return Ok(());
        }
        ensure(Instant::now() < end, "静置专项处理超时")?;
        tick(cx).await;
    }
}
async fn failed(pin: WindowHandle<PinView>, previous: u64, cx: &mut AsyncApp) -> Result<Value> {
    let end = Instant::now() + WAIT;
    loop {
        let p = probe(pin, cx)?;
        if count(&p, "snapshot_failures")? == previous + 1 && !busy(&p) {
            ensure(
                pin.update(cx, |v, _, _| {
                    !v.saving && v.status.starts_with("工作区保存失败：")
                })?,
                "保存失败未恢复可重试窗口",
            )?;
            ensure(!pending(&p), "保存失败残留 pending_close")?;
            return Ok(p);
        }
        ensure(Instant::now() < end, "只读清单没有实际失败 ACK")?;
        tick(cx).await;
    }
}
struct ReadOnly(PathBuf, std::fs::Permissions);
impl ReadOnly {
    fn new(path: PathBuf) -> Result<Self> {
        let original = std::fs::metadata(&path)?.permissions();
        let mut changed = original.clone();
        changed.set_readonly(true);
        std::fs::set_permissions(&path, changed)?;
        Ok(Self(path, original))
    }
}
impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, self.1.clone());
    }
}
fn no_temporary_assets(directory: &Path) -> Result<()> {
    ensure(
        !std::fs::read_dir(directory)?
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp")),
        "失败后残留临时资产",
    )
}

struct Owned {
    pin: WindowHandle<PinView>,
    id: String,
    handle: usize,
    original: Weak<gpui::RenderImage>,
    entity: WeakEntity<PinView>,
}
impl Owned {
    async fn open(cx: &mut AsyncApp) -> Result<Self> {
        let pin = geometry::open(WIDTH, false, cx).await?;
        let (id, handle, original, entity) = pin.update(cx, |v, w, cx| -> Result<_> {
            Ok((
                v.workspace
                    .as_ref()
                    .ok_or("静置 Pin 未接入真实工作区")?
                    .session
                    .id
                    .clone(),
                hwnd(w).0 as usize,
                Arc::downgrade(&v.original),
                cx.entity().downgrade(),
            ))
        })??;
        let result = Self {
            pin,
            id,
            handle,
            original,
            entity,
        };
        let p = saved(pin, &result.id, None, cx).await?;
        ensure(
            count(&p, "snapshots_started")? == 1
                && count(&p, "snapshots_committed")? == 1
                && count(&p, "snapshot_failures")? == 0,
            "首存没有立即完成唯一快照",
        )?;
        Ok(result)
    }
    fn native_close(&self) -> Result<u64> {
        let requested = unix_ms()?;
        unsafe {
            PostMessageW(
                Some(HWND(self.handle as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )?;
        }
        Ok(requested)
    }
    fn exists(&self) -> bool {
        unsafe { IsWindow(Some(HWND(self.handle as *mut _))) }.as_bool()
    }
}

/// 两个自有 HWND 的不透明稳定块；完整 BGRA 和 alpha 由 CPU 行 oracle 验证。
async fn framebuffer(
    pin: WindowHandle<PinView>,
    oracle: Oracle,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    presented(pin, cx).await?;
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
    ensure(
        size == (WIDTH, HEIGHT) && client.0 > 0 && client.1 > 0,
        "静置 framebuffer 投影尺寸错误",
    )?;
    let work = crate::pin_native::work_area((placement.x, placement.y));
    let ratio =
        (f64::from(client.0) / f64::from(WIDTH)).min(f64::from(client.1) / f64::from(HEIGHT));
    let offset = (
        (f64::from(client.0) - f64::from(WIDTH) * ratio) / 2.,
        (f64::from(client.1) - f64::from(HEIGHT) * ratio) / 2.,
    );
    let mut samples = Vec::new();
    for source_x in [128, 640] {
        let source_y = 31 * 512 + 256;
        let tx = if oracle.flip_x {
            WIDTH - 1 - source_x
        } else {
            source_x
        };
        let ty = source_y;
        let px = (offset.0 + (f64::from(tx) + 0.5) * ratio).floor() as u32;
        let py = (offset.1 + (f64::from(ty) + 0.5) * ratio).floor() as u32;
        ensure(
            px >= 4 && py >= 4 && px + 4 < client.0 && py + 4 < client.1,
            "静置 GPU 采样靠近边界",
        )?;
        let rect = [px - 1, py - 1, 3, 3];
        ensure(
            i64::from(placement.x) + i64::from(rect[0]) >= i64::from(work.x)
                && i64::from(placement.y) + i64::from(rect[1]) >= i64::from(work.y)
                && i64::from(placement.x) + i64::from(rect[0] + 3)
                    <= i64::from(work.x) + i64::from(work.width)
                && i64::from(placement.y) + i64::from(rect[1] + 3)
                    <= i64::from(work.y) + i64::from(work.height),
            "静置 GPU 样本不在自有可见工作区",
        )?;
        let expected = oracle.pixel(tx, ty);
        ensure(expected[3] == 255, "静置 GPU 小样不是不透明块")?;
        let bounds = [
            ((f64::from(rect[0]) - offset.0) / ratio).floor() - 2.,
            ((f64::from(rect[1]) - offset.1) / ratio).floor() - 2.,
            ((f64::from(rect[0] + 3) - offset.0) / ratio).ceil() + 2.,
            ((f64::from(rect[1] + 3) - offset.1) / ratio).ceil() + 2.,
        ];
        ensure(
            bounds[0] >= 0.
                && bounds[1] >= 0.
                && bounds[2] <= f64::from(WIDTH)
                && bounds[3] <= f64::from(HEIGHT),
            "静置 GPU 过滤裕量越界",
        )?;
        let [x0, y0, x1, y1] = bounds.map(|v| v as u32);
        ensure(x0 < x1 && y0 < y1, "静置 GPU 采样范围为空")?;
        for y in y0..y1 {
            for x in x0..x1 {
                ensure(
                    oracle.pixel(x, y) == expected,
                    "静置 GPU 采样跨颜色/alpha 块",
                )?;
            }
        }
        pin.update(cx, |_, w, _| w.request_frame_capture(rect))??;
        let end = Instant::now() + WAIT;
        let bytes = loop {
            tick(cx).await;
            if let Some(result) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
                break result.map_err(|e| e.to_string())?;
            }
            ensure(Instant::now() < end, "静置 GPU 小样读回超时")?;
        };
        ensure(bytes.len() == 36, "静置 GPU 小样长度错误")?;
        let difference = bytes
            .chunks_exact(4)
            .flat_map(|p| p.iter().zip(expected).map(|(&a, b)| a.abs_diff(b)))
            .max()
            .unwrap_or(0);
        ensure(difference <= 1, &format!("静置 GPU 稳定块错误：{difference}; {oracle:?}; actual={bytes:?}; expected={expected:?}"))?;
        samples.push(json!({"source_anchor":[source_x,source_y],"output_anchor":[tx,ty],"rect":rect,"expected_bgra":expected,"actual_bgra":bytes,"max_difference":difference,"tolerance":1,"source":"owned-pin-framebuffer","source_footprint":[x0,y0,x1,y1]}));
    }
    Ok(samples)
}

/// 只有便宜的完成探针；下一操作与上一操作的完成观察在同一 UI 更新。
async fn rapid(owned: &Owned, expected_busy: bool, cx: &mut AsyncApp) -> Result<Value> {
    let pin = owned.pin;
    let base = probe(pin, cx)?;
    ensure(busy(&base) == expected_busy, "快速开始前真实 busy 状态错误")?;
    let started = Instant::now();
    let start_unix = unix_ms()?;
    let actions = [
        PinAction::Brightness(10),
        PinAction::FlipX,
        PinAction::Undo,
        PinAction::Redo,
    ];
    let names = ["brightness-20", "flip-x", "undo-flip", "redo-flip"];
    let mut steps = Vec::new();
    let mut previous_request = pin.update(cx, |v, _, _| v.processing_probe().request)?;
    let mut old = pin.update(cx, |v, w, cx| -> Result<_> {
        ensure(
            !v.processing && v.workspace_snapshot_busy() == expected_busy,
            "快速首步状态改变",
        )?;
        let old = Arc::downgrade(&v.image);
        v.perform(actions[0], w, cx);
        ensure(
            v.processing_probe().request > previous_request,
            "快速首步没有发起处理",
        )?;
        Ok(old)
    })??;
    for index in 0..actions.len() {
        let end = Instant::now() + WAIT;
        loop {
            let result = pin.update(cx, |v, w, cx| -> Result<Option<_>> {
                if v.processing { return Ok(None); }
                let p = v.processing_probe();
                let workspace = serde_json::to_value(v.workspace_snapshot_probe().ok_or("快速操作工作区 probe 丢失")?)?;
                ensure(p.request > previous_request && p.completed && !p.failed && !p.image_unavailable, "快速操作没有真实完成")?;
                let choice = if index == 0 { "unique-refill-buffer" } else { "unique-buffer" };
                ensure(p.reused && p.choice == Some(choice) && p.fallback_reason.is_none() && p.old_cpu_pointer == p.new_cpu_pointer && p.old_buffer_capacity.is_some() && p.old_buffer_capacity == p.new_buffer_capacity, "快速操作新增共享保存来源或没有复用实际 Vec")?;
                ensure(count(&workspace, "snapshots_started")? == count(&base, "snapshots_started")? && count(&workspace, "encode_starts")? == count(&base, "encode_starts")? && busy(&workspace) == expected_busy, "快速操作期间额外启动自动保存")?;
                ensure(old.upgrade().is_none(), "快速处理后旧图仍存活")?;
                let completed_ms = started.elapsed().as_millis() as u64;
                let completed_unix = unix_ms()?;
                let mut next = None;
                if index + 1 < actions.len() {
                    let weak = Arc::downgrade(&v.image);
                    v.perform(actions[index + 1], w, cx);
                    ensure(v.processing_probe().request > p.request, "下一快速操作没有同 UI 更新启动")?;
                    next = Some(weak);
                }
                Ok(Some((json!({"step":names[index],"completed_ms":completed_ms,"completed_unix_ms":completed_unix,"next_started_in_same_ui_update":index+1<actions.len(),"processing_probe":p,"workspace_probe":workspace,"old_weak_expired":true}), p.request, next)))
            })??;
            if let Some((step, request, next)) = result {
                steps.push(step);
                previous_request = request;
                if let Some(next) = next {
                    old = next;
                }
                break;
            }
            ensure(Instant::now() < end, "快速操作完成超时")?;
            tick(cx).await;
        }
    }
    let duration = started.elapsed().as_millis() as u64;
    ensure(
        duration < MAX_DELAY_MS,
        "快速 burst 超过最大保存窗口，不能证明静置合并",
    )?;
    let last_completed = unix_ms()?;
    Ok(
        json!({"steps":steps,"duration_ms":duration,"started_unix_ms":start_unix,"last_completed_unix_ms":last_completed,"snapshot_start_delta":0,"encode_start_delta":0,"shared_fallbacks":0,"unique_buffer_reuses":4,"unique_refills":1,"unique_geometry_reuses":3,"gap_between_completed_and_next_start_ms":0,"full_pixel_checks_inside_burst":0,"save_waits_inside_burst":0,"workspace_before":base,"workspace_after":probe(pin,cx)?}),
    )
}

async fn idle_unchanged(
    pin: WindowHandle<PinView>,
    baseline: &Value,
    duration: Duration,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let end = Instant::now() + duration;
    loop {
        let p = probe(pin, cx)?;
        ensure(
            count(&p, "snapshots_started")? == count(baseline, "snapshots_started")?
                && count(&p, "snapshots_committed")? == count(baseline, "snapshots_committed")?
                && !busy(&p),
            "静置 ACK 后重复保存同一内容",
        )?;
        if Instant::now() >= end {
            return Ok(p);
        }
        tick(cx).await;
    }
}

async fn closed(
    owned: Owned,
    current: Weak<gpui::RenderImage>,
    expected: Oracle,
    case: u8,
    request_ms: u64,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<u64> {
    let end = Instant::now() + WAIT;
    while owned.original.upgrade().is_some()
        || current.upgrade().is_some()
        || owned.entity.upgrade().is_some()
    {
        ensure(Instant::now() < end, "静置专项关闭未释放原图/当前图/实体")?;
        tick(cx).await;
    }
    geometry::wait_hwnd_destroyed(owned.handle, cx).await?;
    let destroyed_ms = unix_ms()?;
    ensure(
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<PinView>().is_none())
        })?,
        "静置专项关闭后还有 Pin 实体",
    )?;
    let final_entry = entry(&owned.id)?;
    ensure(
        final_entry.status == PinStatus::Closed
            && final_entry.state.adjustments == Adjustments::default(),
        "关闭工作区未保存 canonical 状态",
    )?;
    let path = asset(&owned.id)?;
    verify_png(&path, expected)?;
    let saved = archive(
        report,
        &format!("case-{case}-final.png"),
        &owned.id,
        expected,
    )?;
    ensure(
        crate::long_capture_runtime::active_workers() == 0
            && crate::long_capture_pipeline::active_capture_threads() == 0,
        "静置关闭后仍有采集线程/消费者",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    let memory = cpu_memory()?;
    let private = memory["private_bytes"]
        .as_u64()
        .ok_or("关闭内存缺少 private_bytes")?;
    report.record("workspace-idle-case-closed", json!({"case":case,"entry_id":owned.id,"hwnd":owned.handle,"native_close_request_unix_ms":request_ms,"hwnd_destroyed_observed_unix_ms":destroyed_ms,"final_png":saved,"final_png_dimensions":[WIDTH,HEIGHT],"final_png_all_pixels_and_alpha_checked":true,"original_weak_expired":true,"current_weak_expired":true,"entity_weak_expired":true,"entity_removed":true,"owned_hwnd_destroyed":true,"status":"closed","canonical_adjustments_reset":true,"capture_workers":0,"capture_threads":0,"consumer_sessions":0,"private_bytes_after_500ms":private,"expected_brightness":expected.brightness,"expected_flip_x":expected.flip_x,"gpu_atlas_residency_directly_queried":false}))?;
    Ok(private)
}
async fn close_clean(
    owned: Owned,
    expected: Oracle,
    case: u8,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<u64> {
    let final_pause = Pause::new(false);
    final_pause.install(owned.pin, cx)?;
    let before = probe(owned.pin, cx)?;
    let request = owned.native_close()?;
    let final_probe = stage(&final_pause, 2, owned.pin, cx).await?;
    ensure(
        pending(&final_probe)
            && count(&final_probe, "snapshots_started")?
                == count(&before, "snapshots_started")? + 1
            && owned.exists(),
        "正常关闭没有唯一最终保存",
    )?;
    report.record("workspace-idle-clean-close", json!({"case":case,"workspace_before":before,"workspace_at_final_encode":final_probe,"native_close_request_unix_ms":request,"hwnd":owned.handle,"window_retained_before_final_ack":true}))?;
    let current = owned.pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
    final_pause.release_commit();
    closed(owned, current, expected, case, request, report, cx).await
}

async fn case_rapid_idle(report: &mut Report, cx: &mut AsyncApp) -> Result<u64> {
    let owned = Owned::open(cx).await?;
    let initial = entry(&owned.id)?;
    geometry::perform_after_save(owned.pin, PinAction::Brightness(10), cx).await?;
    processed(owned.pin, cx).await?;
    let warm = saved(owned.pin, &owned.id, initial.image_file.as_deref(), cx).await?;
    ready(owned.pin, cx).await?;
    let old_entry = entry(&owned.id)?;
    let pause = Pause::new(true);
    pause.install(owned.pin, cx)?;
    let rapid_trace = rapid(&owned, false, cx).await?;
    let at_start = stage(&pause, 1, owned.pin, cx).await?;
    // 完成只能由25ms只读轮询观察；实际policy起点可能早于此观察，保留50ms误差界。
    let observed_idle_ms = count(&at_start, "last_started_unix_ms")?
        .saturating_sub(count(&rapid_trace, "last_completed_unix_ms")?);
    ensure(
        observed_idle_ms >= QUIET_MS - 50,
        "最新 burst 后未经过真实静置窗口",
    )?;
    ensure(
        count(&at_start, "snapshots_started")? == count(&warm, "snapshots_started")? + 1
            && count(&at_start, "snapshots_committed")? == count(&warm, "snapshots_committed")?,
        "静置没有唯一最新保存",
    )?;
    ensure(
        entry(&owned.id)?.image_file == old_entry.image_file,
        "编码前清单已提前提交",
    )?;
    verify_pixels(owned.pin, bright(20, true), cx)?;
    let gpu = framebuffer(owned.pin, bright(20, true), cx).await?;
    pause.release_encode();
    let encoded = stage(&pause, 2, owned.pin, cx).await?;
    ensure(
        count(&encoded, "encode_finishes")? == count(&warm, "encode_finishes")? + 1,
        "静置流式编码没有唯一完成",
    )?;
    pause.release_commit();
    let committed = saved(owned.pin, &owned.id, old_entry.image_file.as_deref(), cx).await?;
    ensure(
        count(&committed, "snapshots_started")? == count(&warm, "snapshots_started")? + 1
            && count(&committed, "snapshots_committed")?
                == count(&warm, "snapshots_committed")? + 1,
        "静置没有唯一最新提交",
    )?;
    verify_png(&asset(&owned.id)?, bright(20, true))?;
    let idle_png = archive(report, "case-1-idle.png", &owned.id, bright(20, true))?;
    let idle = idle_unchanged(owned.pin, &committed, Duration::from_millis(1_000), cx).await?;
    report.record("workspace-idle-rapid-coalesced", json!({"case":1,"rapid":rapid_trace,"warmup_probe":warm,"started_probe":at_start,"encoded_probe":encoded,"committed_probe":committed,"idle_probe":idle,"idle_png":idle_png,"observed_idle_before_snapshot_ms":observed_idle_ms,"completion_observation_tolerance_ms":50,"idle_observed_lower_bound_ms":700,"full_cpu_pixels_checked":PIXELS,"original_all_pixels_unchanged":true,"all_alpha_checked":true,"idle_png_all_pixels_checked":true,"framebuffer_samples":gpu,"snapshot_source_acquires_in_burst":0,"idle_latest_commits":1,"duplicate_idle_snapshots":0,"full_pixel_checks_after_burst_only":true}))?;
    close_clean(owned, bright(20, true), 1, report, cx).await
}

async fn case_pending_close(report: &mut Report, cx: &mut AsyncApp) -> Result<u64> {
    let owned = Owned::open(cx).await?;
    let before = probe(owned.pin, cx)?;
    let pause = Pause::new(true);
    pause.install(owned.pin, cx)?;
    owned.pin.update(cx, |v, w, cx| -> Result<()> {
        v.perform(PinAction::Brightness(10), w, cx);
        ensure(v.processing, "pending 关闭之前没有真实处理中")
    })??;
    let requested = owned.native_close()?;
    let end = Instant::now() + WAIT;
    let pending_probe = loop {
        let state = owned.pin.update(cx, |v, _, _| -> Result<_> {
            Ok((
                v.processing,
                v.saving,
                serde_json::to_value(
                    v.workspace_snapshot_probe()
                        .ok_or("pending 关闭工作区 probe 丢失")?,
                )?,
            ))
        })??;
        if state.0 && state.1 && pending(&state.2) {
            ensure(
                !busy(&state.2)
                    && count(&state.2, "snapshots_started")?
                        == count(&before, "snapshots_started")?,
                "processing pending 阶段提前快照",
            )?;
            break state.2;
        }
        ensure(
            Instant::now() < end && pause.0.stage() == 0,
            "没有观察到真实 processing pending 关闭，不能宣称覆盖",
        )?;
        tick(cx).await;
    };
    processed(owned.pin, cx).await?;
    let completed_observed = unix_ms()?;
    let final_probe = stage(&pause, 1, owned.pin, cx).await?;
    ensure(
        pending(&final_probe)
            && count(&final_probe, "snapshots_started")?
                == count(&before, "snapshots_started")? + 1
            && owned.exists(),
        "pending 关闭没有唯一最终快照",
    )?;
    let close_delay =
        count(&final_probe, "last_started_unix_ms")?.saturating_sub(completed_observed);
    ensure(
        close_delay < QUIET_MS,
        "pending 关闭完成处理后仍等待静置期限",
    )?;
    verify_pixels(owned.pin, bright(10, false), cx)?;
    let gpu = framebuffer(owned.pin, bright(10, false), cx).await?;
    report.record("workspace-idle-processing-pending-close", json!({"case":2,"hwnd":owned.handle,"native_close_request_unix_ms":requested,"processing_completed_observed_unix_ms":completed_observed,"snapshot_after_processing_observation_ms":close_delay,"processing_pending_probe":pending_probe,"final_snapshot_probe":final_probe,"processing_pending_observed":true,"final_snapshot_after_processing":true,"close_bypasses_idle_policy":true,"framebuffer_samples":gpu,"full_cpu_pixels_checked":PIXELS,"original_all_pixels_unchanged":true,"all_alpha_checked":true}))?;
    pause.release_encode();
    stage(&pause, 2, owned.pin, cx).await?;
    let current = owned.pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
    pause.release_commit();
    closed(owned, current, bright(10, false), 2, requested, report, cx).await
}

async fn case_busy_close(report: &mut Report, cx: &mut AsyncApp) -> Result<u64> {
    let owned = Owned::open(cx).await?;
    let baseline = entry(&owned.id)?;
    let pause = Pause::new(false);
    pause.install(owned.pin, cx)?;
    geometry::perform_after_save(owned.pin, PinAction::Brightness(10), cx).await?;
    let encoded = stage(&pause, 2, owned.pin, cx).await?;
    ready(owned.pin, cx).await?;
    let old = owned.pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
    let rapid_trace = rapid(&owned, true, cx).await?;
    ensure(old.upgrade().is_none(), "编码完成源仍被保存任务保留")?;
    verify_pixels(owned.pin, bright(20, true), cx)?;
    let gpu = framebuffer(owned.pin, bright(20, true), cx).await?;
    let final_pause = Pause::new(true);
    final_pause.install(owned.pin, cx)?;
    let request = owned.native_close()?;
    let end = Instant::now() + WAIT;
    let closing = loop {
        let p = probe(owned.pin, cx)?;
        if pending(&p) && owned.pin.update(cx, |v, _, _| v.saving)? {
            break p;
        }
        ensure(Instant::now() < end, "busy 关闭没有真实 pending")?;
        tick(cx).await;
    };
    ensure(
        busy(&closing)
            && owned.exists()
            && count(&closing, "snapshots_started")? == count(&encoded, "snapshots_started")?
            && entry(&owned.id)?.image_file == baseline.image_file,
        "busy 关闭未等待旧提交 ACK",
    )?;
    pause.release_commit();
    let latest = stage(&final_pause, 1, owned.pin, cx).await?;
    ensure(
        pending(&latest)
            && count(&latest, "snapshots_started")? == count(&encoded, "snapshots_started")? + 1
            && count(&latest, "snapshots_committed")?
                == count(&encoded, "snapshots_committed")? + 1
            && owned.exists(),
        "旧 ACK 后没有唯一最新 final snapshot",
    )?;
    verify_png(&asset(&owned.id)?, bright(10, false))?;
    let old_png = archive(report, "case-3-old-ack.png", &owned.id, bright(10, false))?;
    report.record("workspace-idle-busy-native-close", json!({"case":3,"hwnd":owned.handle,"native_close_request_unix_ms":request,"encoded_probe":encoded,"rapid":rapid_trace,"closing_probe":closing,"latest_final_probe":latest,"old_committed_png":old_png,"window_retained_before_old_ack":true,"old_source_weak_expired":true,"latest_final_snapshots":1,"framebuffer_samples":gpu,"full_cpu_pixels_checked":PIXELS,"original_all_pixels_unchanged":true,"all_alpha_checked":true,"old_committed_png_all_pixels_checked":true}))?;
    final_pause.release_encode();
    stage(&final_pause, 2, owned.pin, cx).await?;
    let current = owned.pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
    final_pause.release_commit();
    closed(owned, current, bright(20, true), 3, request, report, cx).await
}

async fn case_failure_retry(report: &mut Report, cx: &mut AsyncApp) -> Result<u64> {
    let owned = Owned::open(cx).await?;
    let initial = entry(&owned.id)?;
    let old_asset = asset(&owned.id)?;
    let directory = old_asset.parent().ok_or("资产没有父目录")?;
    let manifest = directory.join("workspace.json");
    let original_manifest = std::fs::read(&manifest)?;
    let pause = Pause::new(false);
    pause.install(owned.pin, cx)?;
    geometry::perform_after_save(owned.pin, PinAction::Brightness(10), cx).await?;
    let first = stage(&pause, 2, owned.pin, cx).await?;
    ready(owned.pin, cx).await?;
    owned
        .pin
        .update(cx, |v, w, cx| v.perform(PinAction::Brightness(10), w, cx))?;
    processed(owned.pin, cx).await?;
    let processing = owned.pin.update(cx, |v, _, _| v.processing_probe())?;
    ensure(
        processing.reused && processing.choice == Some("unique-refill-buffer"),
        "retry 前处理没有在已释放源上复用",
    )?;
    let retry = Pause::new(false);
    retry.install(owned.pin, cx)?;
    let readonly = ReadOnly::new(manifest.clone())?;
    pause.release_commit();
    let failure = failed(owned.pin, count(&first, "snapshot_failures")?, cx).await?;
    let failure_observed = unix_ms()?;
    let failure_clock = Instant::now();
    ensure(
        std::fs::read(&manifest)? == original_manifest
            && entry(&owned.id)?.image_file == initial.image_file,
        "实际失败改变了旧清单",
    )?;
    no_temporary_assets(directory)?;
    while failure_clock.elapsed() < Duration::from_millis(450) {
        let p = probe(owned.pin, cx)?;
        ensure(
            !busy(&p) && count(&p, "snapshots_started")? == count(&failure, "snapshots_started")?,
            "失败冷却期提前重试",
        )?;
        tick(cx).await;
    }
    drop(readonly);
    let retried = stage(&retry, 2, owned.pin, cx).await?;
    let retry_delay = count(&retried, "last_started_unix_ms")?.saturating_sub(failure_observed);
    ensure(
        count(&retried, "snapshots_started")? == count(&failure, "snapshots_started")? + 1
            && count(&retried, "snapshot_failures")? == count(&failure, "snapshot_failures")?
            && retry_delay >= 900,
        "实际 tick 重试次数或 1s 冷却错误",
    )?;
    ensure(
        owned.exists() && !pending(&retried),
        "自动 retry 没有保留活动窗口",
    )?;
    verify_pixels(owned.pin, bright(20, false), cx)?;
    let gpu = framebuffer(owned.pin, bright(20, false), cx).await?;
    verify_png(&old_asset, Oracle::default())?;
    let old_png = archive(
        report,
        "case-4-preserved-old.png",
        &owned.id,
        Oracle::default(),
    )?;
    ensure(
        std::fs::read(&manifest)? == original_manifest,
        "重试编码后旧清单提前变化",
    )?;
    retry.release_commit();
    let committed = saved(owned.pin, &owned.id, initial.image_file.as_deref(), cx).await?;
    ensure(
        count(&committed, "snapshots_started")? == count(&failure, "snapshots_started")? + 1
            && count(&committed, "snapshots_committed")?
                == count(&failure, "snapshots_committed")? + 1,
        "失败后没有唯一最新成功提交",
    )?;
    verify_png(&asset(&owned.id)?, bright(20, false))?;
    let latest_png = archive(
        report,
        "case-4-retry-latest.png",
        &owned.id,
        bright(20, false),
    )?;
    let idle = idle_unchanged(owned.pin, &committed, Duration::from_millis(1_000), cx).await?;
    report.record("workspace-idle-readonly-failure-real-tick-retry", json!({"case":4,"failure_observed_unix_ms":failure_observed,"retry_start_after_observed_failure_ms":retry_delay,"initial_encoded_probe":first,"failure_probe":failure,"retry_encoded_probe":retried,"committed_probe":committed,"idle_probe":idle,"old_preserved_png":old_png,"latest_retry_png":latest_png,"retry_not_before_observed_failure_ms":900,"old_manifest_exact":true,"old_png_all_pixels_exact":true,"temporary_assets_after_failure":0,"real_readonly_manifest_failure":true,"retry_uses_real_tick":true,"retry_latest_commits":1,"latest_png_all_pixels_checked":true,"framebuffer_samples":gpu,"full_cpu_pixels_checked":PIXELS,"original_all_pixels_unchanged":true,"all_alpha_checked":true}))?;
    close_clean(owned, bright(20, false), 4, report, cx).await
}

async fn case_close_failure(report: &mut Report, cx: &mut AsyncApp) -> Result<u64> {
    let owned = Owned::open(cx).await?;
    let initial = entry(&owned.id)?;
    let old_asset = asset(&owned.id)?;
    let directory = old_asset.parent().ok_or("资产没有父目录")?;
    let manifest = directory.join("workspace.json");
    let original_manifest = std::fs::read(&manifest)?;
    let first = Pause::new(false);
    first.install(owned.pin, cx)?;
    owned
        .pin
        .update(cx, |v, w, cx| v.perform(PinAction::Brightness(10), w, cx))?;
    processed(owned.pin, cx).await?;
    let request = owned.native_close()?;
    let encoding = stage(&first, 2, owned.pin, cx).await?;
    ensure(
        pending(&encoding) && owned.pin.update(cx, |v, _, _| v.saving)? && owned.exists(),
        "关闭失败专项没有实际 final-close 编码",
    )?;
    let readonly = ReadOnly::new(manifest.clone())?;
    first.release_commit();
    let failure = failed(owned.pin, count(&encoding, "snapshot_failures")?, cx).await?;
    let failed_at = unix_ms()?;
    ensure(
        owned.exists()
            && std::fs::read(&manifest)? == original_manifest
            && entry(&owned.id)?.image_file == initial.image_file,
        "关闭失败没有保留原窗口和旧清单",
    )?;
    no_temporary_assets(directory)?;
    let retry = Pause::new(false);
    retry.install(owned.pin, cx)?;
    drop(readonly);
    let request_again = owned.native_close()?;
    let latest = stage(&retry, 2, owned.pin, cx).await?;
    ensure(
        pending(&latest)
            && owned.exists()
            && count(&latest, "snapshots_started")? == count(&failure, "snapshots_started")? + 1
            && count(&latest, "snapshot_failures")? == count(&failure, "snapshot_failures")?,
        "第二次原生关闭没有唯一最终重试",
    )?;
    let retry_started = count(&latest, "last_started_unix_ms")?;
    ensure(
        retry_started >= request_again && retry_started.saturating_sub(request_again) < QUIET_MS,
        "用户再次关闭没有绕过静置/失败冷却",
    )?;
    verify_pixels(owned.pin, bright(10, false), cx)?;
    let gpu = framebuffer(owned.pin, bright(10, false), cx).await?;
    verify_png(&old_asset, Oracle::default())?;
    let old_png = archive(
        report,
        "case-5-preserved-old.png",
        &owned.id,
        Oracle::default(),
    )?;
    report.record("workspace-idle-close-failure-retained-reclose", json!({"case":5,"hwnd":owned.handle,"first_native_close_request_unix_ms":request,"failure_observed_unix_ms":failed_at,"second_native_close_request_unix_ms":request_again,"failed_final_probe":encoding,"failure_probe":failure,"retry_final_probe":latest,"old_preserved_png":old_png,"saving_busy_pending_reset_after_failure":true,"window_retained_after_failed_close":true,"old_manifest_exact":true,"old_png_all_pixels_exact":true,"temporary_assets_after_failure":0,"second_close_bypasses_quiet_and_retry":true,"framebuffer_samples":gpu,"full_cpu_pixels_checked":PIXELS,"original_all_pixels_unchanged":true,"all_alpha_checked":true}))?;
    let current = owned.pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
    retry.release_commit();
    closed(
        owned,
        current,
        bright(10, false),
        5,
        request_again,
        report,
        cx,
    )
    .await
}

pub(super) async fn verify(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut preferences = crate::preferences::Preferences::load();
    preferences.pin_restore = true;
    preferences.save()?;
    let closed = vec![
        case_rapid_idle(report, cx).await?,
        case_pending_close(report, cx).await?,
        case_busy_close(report, cx).await?,
        case_failure_retry(report, cx).await?,
        case_close_failure(report, cx).await?,
    ];
    let growth = closed
        .iter()
        .copied()
        .max()
        .ok_or("没有静置关闭内存样本")?
        .saturating_sub(closed[0]);
    report.record("workspace-idle-close-memory-gate", json!({"cases":5,"private_bytes":closed,"growth_bytes":growth,"limit_bytes":CLOSE_LIMIT,"ok":growth<=CLOSE_LIMIT}))?;
    ensure(
        growth <= CLOSE_LIMIT,
        "五项工作区静置关闭内存增长超过原16MiB门",
    )?;
    report.groups.push(json!({"name":"workspace-idle","ok":true,"rounds":1,"cases":5,"source_dimensions":[WIDTH,HEIGHT],"quiet_ms":QUIET_MS,"max_delay_ms":MAX_DELAY_MS,"rapid_steps":8,"rapid_unique_buffer_reuses":8,"rapid_unique_refills":2,"rapid_unique_geometry_reuses":6,"rapid_shared_fallbacks":0,"snapshot_source_acquires_in_rapid_bursts":0,"idle_latest_snapshot_commit_checked":true,"processing_pending_native_close_checked":true,"busy_native_close_latest_final_checked":true,"real_readonly_manifest_failure_and_tick_retry_checked":true,"failed_native_close_retained_and_reclose_checked":true,"full_cpu_and_final_png_pixels_checked_each_case":true,"all_alpha_checked":true,"original_all_pixels_unchanged":true,"expected_image_materialized":false,"gpu_all_pixels_checked":false,"gpu_stable_blocks_per_case":2,"close_private_bytes":closed,"close_private_growth_bytes":growth,"close_private_limit_bytes":CLOSE_LIMIT,"clipboard_untouched":true,"desktop_pixels_read":false,"global_input_sent":false,"actual_tick_and_manifest_commit_tested":true,"gpu_atlas_residency_directly_queried":false,"limitations":["同一工作进程五个有界原生合成图 case；不是旧三轮 overlap/native-browser suite 的重跑", "QA屏障仅延长真实编码/提交以观察竞态，不改变启动 policy 或替代 tick", "完整 BGRA/alpha 和 PNG 逐行验证；GPU 仅自有窗口两个稳定块，未直接查询 atlas 驻留", "快速 burst 小于3s；连续编辑超过最大延迟的保护由策略定向测试验证，非本原生组结论", "只覆盖本机实际DPI和32k自有合成图，不证明长期无泄漏或任意真实网页"]}));
    report.save(None, false)
}
