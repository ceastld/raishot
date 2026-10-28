//! 真正的工作区 tick、PNG、清单提交与处理重叠；仅观察自有合成贴图。
use super::{ensure, geometry, ready, refill, tick, Report, WAIT};
use crate::{
    gpui_capture::hwnd,
    process_memory::cpu_memory,
    pin_model::Adjustments,
    pin_view::{PinAction, PinView},
    pin_workspace::{self as store, PinEntry, PinStatus},
    pin_workspace_runtime::SnapshotPause,
    Result,
};
use gpui::{AsyncApp, WindowHandle};
use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_CLOSE},
};

const WIDTH: u32 = 960;
const CLOSE_LIMIT: u64 = 16 * 1024 * 1024;

fn oracle(brightness: i16, turns: u8) -> refill::Oracle {
    refill::Oracle::processed(brightness, turns)
}
fn entry(id: &str) -> Result<PinEntry> {
    store::load()?
        .pins
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| "重叠专项的工作区记录丢失".into())
}
fn asset(id: &str) -> Result<PathBuf> {
    let entry = entry(id)?;
    store::asset_path(entry.image_file.as_deref().ok_or("工作区快照尚未提交")?)
}
/// 解码每一行并按独立世界坐标逐像素比较，不分配第二张 117 MiB 期望图。
fn verify_png(path: &Path, expected: refill::Oracle) -> Result<()> {
    let mut reader = png::Decoder::new(BufReader::new(File::open(path)?)).read_info()?;
    let info = reader.info();
    let size = expected.dimensions(WIDTH);
    ensure(
        (info.width, info.height) == size
            && info.color_type == png::ColorType::Rgba
            && info.bit_depth == png::BitDepth::Eight,
        "工作区 PNG 尺寸或格式错误",
    )?;
    let mut y = 0;
    while let Some(row) = reader.next_row()? {
        ensure(
            row.data().len() == size.0 as usize * 4,
            "工作区 PNG 行字节数错误",
        )?;
        for (x, rgba) in row.data().chunks_exact(4).enumerate() {
            let bgra = expected.pixel(x as u32, y, WIDTH);
            if rgba != [bgra[2], bgra[1], bgra[0], bgra[3]] {
                return Err(format!("工作区 PNG 全像素/alpha 错误 ({x},{y})").into());
            }
        }
        y += 1;
    }
    ensure(y == size.1, "工作区 PNG 行数不完整")
}
async fn wait_stage(
    pause: &SnapshotPause,
    stage: u8,
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    while pause.stage() != stage {
        ensure(
            Instant::now() < deadline,
            "真实 workspace tick 未到预期编码阶段",
        )?;
        tick(cx).await;
    }
    ensure(
        pin.update(cx, |v, _, _| v.workspace_snapshot_busy())?,
        "编码阶段没有真实 busy",
    )
}
async fn wait_saved(
    pin: WindowHandle<PinView>,
    id: &str,
    previous: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let busy = pin.update(cx, |v, _, _| v.workspace_snapshot_busy())?;
        let saved = entry(id)?;
        if !busy
            && saved
                .image_file
                .as_deref()
                .is_some_and(|p| Some(p) != previous)
        {
            return Ok(());
        }
        ensure(
            Instant::now() < deadline,
            "工作区实际 PNG/清单提交 ACK 超时",
        )?;
        tick(cx).await;
    }
}
async fn wait_weak(old: &Weak<gpui::RenderImage>, cx: &mut AsyncApp) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    while old.upgrade().is_some() {
        ensure(Instant::now() < deadline, "PNG 编完后旧快照仍持有完整图")?;
        tick(cx).await;
    }
    Ok(())
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

pub(super) async fn verify(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut preferences = crate::preferences::Preferences::load();
    preferences.pin_restore = true;
    preferences.save()?;
    let mut closed = Vec::new();
    let mut fallback_hits = 0;
    let mut busy_reuses = 0;
    for round in 1..=3 {
        let pin = geometry::open(WIDTH, false, cx).await?;
        let (id, original, handle) = pin.update(cx, |v, w, _| -> Result<_> {
            Ok((
                v.workspace
                    .as_ref()
                    .ok_or("实际贴图未附着工作区")?
                    .session
                    .id
                    .clone(),
                Arc::downgrade(&v.original),
                hwnd(w).0 as usize,
            ))
        })??;
        wait_saved(pin, &id, None, cx).await?;
        let initial = entry(&id)?;
        let initial_asset = asset(&id)?;
        verify_png(&initial_asset, oracle(0, 0))?;
        let manifest = initial_asset
            .parent()
            .ok_or("专项资产没有父目录")?
            .join("workspace.json");
        let initial_manifest = std::fs::read(&manifest)?;
        let first = SnapshotPause::new(true);
        geometry::perform_after_save(pin, PinAction::Brightness(10), cx).await?;
        pin.update(cx, |v, _, _| {
            v.pause_next_workspace_snapshot_for_qa(first.clone())
        })??;
        ready(pin, cx).await?;
        refill::verify_pixels(pin, oracle(10, 0), WIDTH, cx)?;
        let saved_ten = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
        wait_stage(&first, 1, pin, cx).await?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            ensure(v.workspace_snapshot_busy(), "共享保存与调色未实际重叠")?;
            v.perform(PinAction::Brightness(10), w, cx);
            Ok(())
        })??;
        ready(pin, cx).await?;
        refill::verify_pixels(pin, oracle(20, 0), WIDTH, cx)?;
        let fallback = pin.update(cx, |v, _, _| v.processing_probe())?;
        ensure(
            !fallback.reused
                && fallback.completed
                && !fallback.failed
                && fallback.fallback_reason == Some("shared-image"),
            "编码前的真实共享快照没有安全回退",
        )?;
        {
            let saved = saved_ten.upgrade().ok_or("编码前共享快照过早释放")?;
            oracle(10, 0).verify(
                (WIDTH, 32_000),
                saved.as_bytes(0).ok_or("旧共享快照无像素")?,
                WIDTH,
            )?;
        }
        fallback_hits += 1;
        report.record("workspace-before-encode-fallback", serde_json::json!({"round":round,"hwnd":handle,"entry_id":id,"actual_tick_started":true,"workspace_busy":true,"probe":fallback,"old_snapshot_all_pixels_unchanged":true}))?;
        first.release_encode();
        wait_stage(&first, 2, pin, cx).await?;
        wait_weak(&saved_ten, cx).await?;
        report.record("workspace-encoded-source-released", serde_json::json!({"round":round,"workspace_busy":true,"old_source_weak_expired":true,"manifest_not_committed":entry(&id)?.image_file == initial.image_file}))?;

        let second = SnapshotPause::new(false);
        pin.update(cx, |v, _, _| {
            v.pause_next_workspace_snapshot_for_qa(second.clone())
        })??;
        if round == 3 {
            // 真实只读清单使事务失败；旧 PNG/清单保留，原定 tick 自动重试最新内容。
            let guard = ReadOnly::new(manifest.clone())?;
            first.release_commit();
            let deadline = Instant::now() + WAIT;
            loop {
                if pin.update(cx, |v, _, _| {
                    !v.workspace_snapshot_busy() && v.status.starts_with("工作区保存失败：")
                })? {
                    break;
                }
                ensure(Instant::now() < deadline, "只读清单没有产生实际保存失败")?;
                tick(cx).await;
            }
            ensure(
                std::fs::read(&manifest)? == initial_manifest
                    && entry(&id)?.image_file == initial.image_file,
                "保存失败改变了旧清单",
            )?;
            ensure(
                !std::fs::read_dir(initial_asset.parent().unwrap())?
                    .filter_map(|p| p.ok())
                    .any(|p| p.file_name().to_string_lossy().ends_with(".tmp")),
                "保存失败残留临时资产",
            )?;
            drop(guard);
            // 先量失败 ACK 的清理并解除故障，再做全图检查；期间允许正常重试，
            // 下一快照的提交屏障会保留旧资产，不能把它的新临时文件算作失败残留。
            verify_png(&initial_asset, oracle(0, 0))?;
            report.record("workspace-manifest-failure-kept-old", serde_json::json!({"round":round,"old_manifest_exact":true,"old_png_all_pixels_exact":true,"temporary_assets":0,"window_retained":true,"retry_uses_real_tick":true}))?;
        } else {
            first.release_commit();
            wait_saved(pin, &id, initial.image_file.as_deref(), cx).await?;
            verify_png(&asset(&id)?, oracle(10, 0))?;
        }
        wait_stage(&second, 2, pin, cx).await?;
        let old_twenty = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
        for (step, action, expected) in [
            ("rotate-90", PinAction::Rotate(1), oracle(20, 1)),
            ("undo-rotation", PinAction::Undo, oracle(20, 0)),
            ("redo-rotation", PinAction::Redo, oracle(20, 1)),
        ] {
            let old = pin.update(cx, |v, w, cx| -> Result<_> {
                ensure(
                    v.workspace_snapshot_busy() && second.stage() == 2,
                    "提交在途与处理未实际重叠",
                )?;
                let old = Arc::downgrade(&v.image);
                v.perform(action, w, cx);
                Ok(old)
            })??;
            ready(pin, cx).await?;
            refill::verify_pixels(pin, expected, WIDTH, cx)?;
            let gpu = refill::framebuffer(pin, expected, cx).await?;
            let probe = pin.update(cx, |v, _, _| v.processing_probe())?;
            ensure(
                probe.completed
                    && !probe.failed
                    && probe.reused
                    && probe.choice == Some("unique-refill-buffer")
                    && probe.old_cpu_pointer == probe.new_cpu_pointer
                    && probe.old_buffer_capacity == probe.new_buffer_capacity
                    && probe.old_buffer_capacity.is_some(),
                "PNG 后保存仍在途时未复用真实 Vec",
            )?;
            ensure(old.upgrade().is_none(), "处理后旧共享源仍存活")?;
            busy_reuses += 1;
            report.record("workspace-commit-busy-reuse", serde_json::json!({"round":round,"step":step,"workspace_busy":true,"all_pixels_checked":30_720_000u64,"all_alpha_checked":true,"original_all_pixels_unchanged":true,"probe":probe,"framebuffer_samples":gpu,"old_weak_expired":true}))?;
        }
        ensure(old_twenty.upgrade().is_none(), "PNG 后初始当前图没有释放")?;
        let current = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
        // 真 WM_CLOSE 进入生产关闭回调；在自动快照的提交屏障之后取最终旋转快照。
        unsafe {
            PostMessageW(Some(HWND(handle as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0))?;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            if pin.update(cx, |v, _, _| v.saving && v.workspace_snapshot_busy())? {
                break;
            }
            ensure(Instant::now() < deadline, "原生关闭没有等待在途快照")?;
            tick(cx).await;
        }
        report.record("workspace-native-close-waits", serde_json::json!({"round":round,"hwnd":handle,"owned_hwnd_exists_before_ack":unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool(),"workspace_busy":true,"close_requested_native":true}))?;
        second.release_commit();
        let deadline = Instant::now() + WAIT;
        while original.upgrade().is_some() || current.upgrade().is_some() {
            ensure(Instant::now() < deadline, "最终工作区关闭未释放完整图片")?;
            tick(cx).await;
        }
        geometry::wait_hwnd_destroyed(handle, cx).await?;
        ensure(
            cx.update(|cx| {
                cx.windows()
                    .into_iter()
                    .all(|w| w.downcast::<PinView>().is_none())
            })?,
            "关闭后仍有贴图实体",
        )?;
        let final_entry = entry(&id)?;
        ensure(
            final_entry.status == PinStatus::Closed
                && final_entry.state.adjustments == Adjustments::default(),
            "最终关闭记录没有保存 canonical 状态",
        )?;
        verify_png(&asset(&id)?, oracle(20, 1))?;
        ensure(
            crate::long_capture_runtime::active_workers() == 0
                && crate::long_capture_pipeline::active_capture_threads() == 0,
            "关闭后仍有采集工作线程",
        )?;
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        let memory = cpu_memory()?;
        let private = memory["private_bytes"]
            .as_u64()
            .ok_or("关闭内存缺少 private_bytes")?;
        closed.push(private);
        report.record("workspace-overlap-round-closed", serde_json::json!({"round":round,"hwnd":handle,"entry_id":id,"final_png_dimensions":[32_000,WIDTH],"final_png_all_pixels_and_alpha_checked":true,"status":"closed","canonical_adjustments_reset":true,"original_weak_expired":true,"current_weak_expired":true,"old_snapshot_weaks_expired":true,"entity_removed":true,"owned_hwnd_destroyed":true,"capture_workers":0,"capture_threads":0,"private_bytes_after_500ms":private}))?;
    }
    let growth = closed
        .iter()
        .copied()
        .max()
        .ok_or("缺少三轮关闭数据")?
        .saturating_sub(closed[0]);
    report.record("workspace-overlap-close-memory-gate", serde_json::json!({"private_bytes":closed,"growth_bytes":growth,"limit_bytes":CLOSE_LIMIT,"ok":growth<=CLOSE_LIMIT}))?;
    ensure(
        growth <= CLOSE_LIMIT,
        "真实工作区保存重叠三轮关闭内存增长超过原16MiB门",
    )?;
    ensure(
        fallback_hits == 3 && busy_reuses == 9,
        "实际重叠路径覆盖不完整",
    )?;
    report.groups.push(serde_json::json!({"name":"three-rounds-32000px-workspace-save-processing-overlap","ok":true,"rounds":3,"before_encode_shared_fallbacks":fallback_hits,"commit_busy_unique_refills":busy_reuses,"actual_tick_and_manifest_commit_tested":true,"native_close_final_snapshot_tested":true,"real_manifest_failure_and_tick_retry_tested":true,"old_snapshot_all_pixels_unchanged":true,"final_png_all_pixels_and_alpha_checked":true,"clipboard_untouched":true,"desktop_pixels_read":false,"global_input_sent":false,"limitations":["编码前与编码后设置有限QA屏障，以真实tick/encoder/事务放大竞态窗口，不代表磁盘自然延迟分布", "只有当前实际DPI及三轮自有合成图，不证明长期或第三方真实网页覆盖", "原始PNG和最终PNG逐行验证，未创建恢复窗口"]}));
    report.save(None, false)
}
