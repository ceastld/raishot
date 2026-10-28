//! 绝对原图重填的独占 / QA 强制借用冷进程对照；共享 PNG 另用独立进程。
use super::{
    close, ensure,
    geometry::{
        fixture_pixel, open, perform_after_save, process_cpu_ms, wait_hwnd_destroyed, Begun,
        EncodingJob,
    },
    presented, ready, tick, Report, WAIT,
};
use crate::{
    gpui_capture::hwnd,
    process_memory::cpu_memory,
    pin_view::{PinAction, PinView},
    pin_workspace_runtime::{snapshot_pixels, SnapshotPixels},
    Result,
};
use gpui::{AsyncApp, WindowHandle};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 32_000;
const CLOSE_LIMIT: u64 = 16 * 1024 * 1024;
const REFILL_KINDS: [&str; 9] = [
    "brightness-20",
    "rotate-90",
    "brightness-10-after-90",
    "rotate-to-zero",
    "rotate-270",
    "gray",
    "invert",
    "undo-invert",
    "redo-invert",
];

/// 期望由原始世界坐标和明确的绝对颜色公式生成；不调用生产重填/几何计划。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Oracle {
    pub(super) turns: u8,
    pub(super) brightness: i16,
    grayscale: bool,
    invert: bool,
}
impl Oracle {
    pub(super) fn processed(brightness: i16, turns: u8) -> Self {
        Self {
            brightness,
            turns,
            ..Self::default()
        }
    }
    pub(super) fn dimensions(self, width: u32) -> (u32, u32) {
        if self.turns.is_multiple_of(2) {
            (width, HEIGHT)
        } else {
            (HEIGHT, width)
        }
    }
    fn source(self, x: u32, y: u32, width: u32) -> (u32, u32) {
        match self.turns {
            1 => (y, HEIGHT - 1 - x),
            2 => (width - 1 - x, HEIGHT - 1 - y),
            3 => (width - 1 - y, x),
            _ => (x, y),
        }
    }
    fn target(self, x: u32, y: u32, width: u32) -> (u32, u32) {
        match self.turns {
            1 => (HEIGHT - 1 - y, x),
            2 => (width - 1 - x, HEIGHT - 1 - y),
            3 => (y, width - 1 - x),
            _ => (x, y),
        }
    }
    pub(super) fn pixel(self, x: u32, y: u32, width: u32) -> [u8; 4] {
        let (sx, sy) = self.source(x, y, width);
        let mut pixel = fixture_pixel(sx, sy);
        let gray = ((u32::from(pixel[0]) * 29
            + u32::from(pixel[1]) * 150
            + u32::from(pixel[2]) * 77
            + 128)
            / 256) as u8;
        for channel in &mut pixel[..3] {
            let mut value = i16::from(if self.grayscale { gray } else { *channel });
            if self.invert {
                value = 255 - value;
            }
            *channel = (value + self.brightness * 255 / 100).clamp(0, 255) as u8;
        }
        pixel
    }
    pub(super) fn verify(self, size: (u32, u32), pixels: &[u8], width: u32) -> Result<()> {
        ensure(size == self.dimensions(width), "重填独立 oracle 尺寸不一致")?;
        ensure(
            pixels.len() == size.0 as usize * size.1 as usize * 4,
            "重填像素字节数错误",
        )?;
        for (y, row) in pixels.chunks_exact(size.0 as usize * 4).enumerate() {
            for (x, pixel) in row.chunks_exact(4).enumerate() {
                let expected = self.pixel(x as u32, y as u32, width);
                if pixel != expected {
                    return Err(format!("绝对原图重填 BGRA/alpha 错误 ({x},{y}): actual={pixel:?}, expected={expected:?}, state={self:?}").into());
                }
            }
        }
        Ok(())
    }
}

pub(super) fn verify_pixels(
    pin: WindowHandle<PinView>,
    oracle: Oracle,
    width: u32,
    cx: &mut AsyncApp,
) -> Result<()> {
    pin.update(cx, |v, _, _| -> Result<()> {
        ensure(
            !v.processing && !v.processing_image_unavailable,
            "重填图像仍在处理中",
        )?;
        let actual = v.state.adjustments;
        ensure(
            (
                actual.turns,
                actual.brightness,
                actual.grayscale,
                actual.invert,
                actual.flip_x,
                actual.flip_y,
            ) == (
                oracle.turns,
                oracle.brightness,
                oracle.grayscale,
                oracle.invert,
                false,
                false,
            ),
            "真实颜色/旋转参数没有达到独立预期",
        )?;
        oracle.verify(
            v.image_size,
            v.image.as_bytes(0).ok_or("重填结果没有像素")?,
            width,
        )?;
        Oracle::default().verify(
            v.original_size,
            v.original.as_bytes(0).ok_or("原图没有像素")?,
            width,
        )
    })?
}

/// 只采自有 HWND 两个不透明稳定块；全 BGRA/alpha 另按借用行逐像素证明。
pub(super) async fn framebuffer(
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
        "重填 GPU 投影尺寸为零",
    )?;
    let ratio =
        (f64::from(client.0) / f64::from(size.0)).min(f64::from(client.1) / f64::from(size.1));
    let offset = (
        (f64::from(client.0) - f64::from(size.0) * ratio) / 2.,
        (f64::from(client.1) - f64::from(size.1) * ratio) / 2.,
    );
    let mut samples = Vec::new();
    for source_x in [128, 640] {
        let source_y = 31 * 512 + 256;
        let (tx, ty) = oracle.target(source_x, source_y, WIDTH);
        let px = (offset.0 + (f64::from(tx) + 0.5) * ratio).floor() as u32;
        let py = (offset.1 + (f64::from(ty) + 0.5) * ratio).floor() as u32;
        let context = format!("oracle={oracle:?}, image={size:?}, client={client:?}, placement={placement:?}, work={work:?}, ratio={ratio}, offset={offset:?}, original=({source_x},{source_y}), target=({tx},{ty}), physical=({px},{py})");
        ensure(
            px >= 4 && py >= 4 && px + 4 < client.0 && py + 4 < client.1,
            &format!("重填 GPU 采样中心太靠近客户区边界；{context}"),
        )?;
        let rect = [px - 1, py - 1, 3, 3];
        ensure(
            i64::from(placement.x) + i64::from(rect[0]) >= i64::from(work.x)
                && i64::from(placement.y) + i64::from(rect[1]) >= i64::from(work.y)
                && i64::from(placement.x) + i64::from(rect[0] + 3)
                    <= i64::from(work.x) + i64::from(work.width)
                && i64::from(placement.y) + i64::from(rect[1] + 3)
                    <= i64::from(work.y) + i64::from(work.height),
            &format!("重填自有 framebuffer 样本不在可见工作区；{context}"),
        )?;
        let expected = oracle.pixel(tx, ty, WIDTH);
        ensure(expected[3] == 255, "重填 GPU 小样不是不透明颜色块")?;
        let bounds = [
            ((f64::from(rect[0]) - offset.0) / ratio).floor() - 2.,
            ((f64::from(rect[1]) - offset.1) / ratio).floor() - 2.,
            ((f64::from(rect[0] + 3) - offset.0) / ratio).ceil() + 2.,
            ((f64::from(rect[1] + 3) - offset.1) / ratio).ceil() + 2.,
        ];
        ensure(
            bounds[0] >= 0.
                && bounds[1] >= 0.
                && bounds[2] <= f64::from(size.0)
                && bounds[3] <= f64::from(size.1),
            &format!("重填 GPU 过滤裕量越界：{bounds:?}；{context}"),
        )?;
        let [x0, y0, x1, y1] = bounds.map(|value| value as u32);
        ensure(x0 < x1 && y0 < y1, "重填 GPU 采样覆盖范围为空")?;
        for y in y0..y1 {
            for x in x0..x1 {
                if oracle.pixel(x, y, WIDTH) != expected {
                    return Err(format!(
                        "重填 GPU 采样跨颜色/alpha 块：({x},{y}), footprint={bounds:?}；{context}"
                    )
                    .into());
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
            ensure(Instant::now() < end, "重填自有 GPU 小样读回超时")?;
        };
        ensure(bytes.len() == 36, "重填 GPU 小样字节数错误")?;
        let difference = bytes
            .chunks_exact(4)
            .flat_map(|pixel| pixel.iter().zip(expected).map(|(&a, b)| a.abs_diff(b)))
            .max()
            .unwrap_or(0);
        ensure(difference <= 1, &format!("重填 GPU 稳定块颜色或缓存错误：{difference}, actual={bytes:?}, expected={expected:?}；{context}"))?;
        samples.push(serde_json::json!({"source_anchor":[source_x,source_y],"output_anchor":[tx,ty],"rect":rect,
            "expected_bgra":expected,"actual_bgra":bytes,"max_difference":difference,"tolerance":1,
            "source":"owned-pin-framebuffer","source_footprint":[x0,y0,x1,y1]}));
    }
    Ok(samples)
}

pub(super) async fn verify(
    report: &mut Report,
    cx: &mut AsyncApp,
    force_borrowed: bool,
) -> Result<()> {
    let mut closed = Vec::new();
    let mut unique_count = 0;
    let mut forced_count = 0;
    let mut eligible_count = 0;
    let mut shared_count = 0;
    let mut unique_by_kind = BTreeMap::<&str, u32>::new();
    for round in 1..=3 {
        let pin = open(WIDTH, force_borrowed, cx).await?;
        let (original, handle) = pin.update(cx, |v, w, _| {
            (Arc::downgrade(&v.original), hwnd(w).0 as usize)
        })?;
        verify_pixels(pin, Oracle::default(), WIDTH, cx)?;
        let prime = Oracle {
            brightness: 10,
            ..Default::default()
        };
        let bright = Oracle {
            brightness: 20,
            ..Default::default()
        };
        let quarter = Oracle { turns: 1, ..bright };
        let adjusted = Oracle {
            brightness: 10,
            ..quarter
        };
        let zero = Oracle {
            turns: 0,
            ..adjusted
        };
        let three = Oracle {
            turns: 3,
            ..adjusted
        };
        let gray = Oracle {
            grayscale: true,
            ..three
        };
        let inverted = Oracle {
            invert: true,
            ..gray
        };
        let steps = [
            (
                "prime-brightness-10",
                PinAction::Brightness(10),
                prime,
                false,
            ),
            ("brightness-20", PinAction::Brightness(10), bright, true),
            ("rotate-90", PinAction::Rotate(1), quarter, true),
            (
                "brightness-10-after-90",
                PinAction::Brightness(-10),
                adjusted,
                true,
            ),
            ("rotate-to-zero", PinAction::Rotate(-1), zero, true),
            ("rotate-270", PinAction::Rotate(-1), three, true),
            ("gray", PinAction::Gray, gray, true),
            ("invert", PinAction::Invert, inverted, true),
            ("undo-invert", PinAction::Undo, gray, true),
            ("redo-invert", PinAction::Redo, inverted, true),
            ("reset", PinAction::ResetImage, Oracle::default(), false),
        ];
        let mut life = pin.update(cx, |v, _, _| v.preview_texture_lifecycle())?;
        let mut previous = Oracle::default();
        let mut old_frames = Vec::new();
        let mut round_reuses = 0;
        let mut refill_counter = pin.update(cx, |v, _, _| v.processing_probe().original_refills)?;
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
            let (probe, next_life, image_id, pointer) = pin.update(cx, |v, _, _| {
                (
                    v.processing_probe(),
                    v.preview_texture_lifecycle(),
                    v.image.id.0,
                    v.image
                        .as_bytes(0)
                        .map_or(0, |pixels| pixels.as_ptr() as usize),
                )
            })?;
            ensure(
                probe.completed && !probe.failed && !probe.image_unavailable,
                "重填 probe 未成功完成",
            )?;
            ensure(
                image_id != old_id && next_life.1 == 0 && next_life.3 > life.3,
                "重填没有新 ImageId 或旧预览正常退休",
            )?;
            if step == "reset" {
                ensure(
                    probe.choice == Some("reset-original") && image_id == original_id,
                    "重填 Reset 没有回到原图 ID",
                )?;
                ensure(
                    probe.original_refills == refill_counter,
                    "Reset 错误累加重填次数",
                )?;
            } else {
                ensure(
                    probe.new_image_id == image_id
                        && probe.new_cpu_pointer == pointer
                        && probe.old_image_id == old_id
                        && probe.old_byte_length == (WIDTH * HEIGHT * 4) as usize
                        && probe.new_byte_length == probe.old_byte_length,
                    "重填 probe 没有绑定真实 pointer/id/字节数",
                )?;
                if eligible {
                    eligible_count += 1;
                }
                if probe.reused {
                    ensure(
                        eligible
                            && !force_borrowed
                            && probe.choice == Some("unique-refill-buffer")
                            && probe.fallback_reason.is_none()
                            && probe.old_cpu_pointer == probe.new_cpu_pointer
                            && probe.old_buffer_capacity.is_some()
                            && probe.old_buffer_capacity == probe.new_buffer_capacity
                            && probe
                                .old_buffer_capacity
                                .is_some_and(|capacity| capacity >= probe.old_byte_length)
                            && probe.original_refills == refill_counter + 1,
                        "重填没有真实消费相同 Vec pointer/len/capacity 或重填计数错误",
                    )?;
                    unique_count += 1;
                    round_reuses += 1;
                    *unique_by_kind.entry(step).or_default() += 1;
                } else {
                    ensure(
                        probe.old_cpu_pointer != probe.new_cpu_pointer
                            && probe.choice == Some("borrowed-original")
                            && probe.original_refills == refill_counter,
                        "借用回退错误报告复用或重填次数",
                    )?;
                    if eligible && force_borrowed {
                        ensure(
                            probe.fallback_reason == Some("qa-forced-borrowed"),
                            "重填借用控制组未真实选择 QAForced",
                        )?;
                        forced_count += 1;
                    } else if eligible {
                        ensure(
                            probe.fallback_reason == Some("shared-image"),
                            "重填独占缺失并且没有实际 Shared 证据",
                        )?;
                        shared_count += 1;
                    } else {
                        ensure(
                            probe.fallback_reason == Some("original-image"),
                            "首笔颜色处理没有借用受保护原图",
                        )?;
                    }
                }
            }
            let old_expired = old.upgrade().is_none();
            let shared_old_all_pixels_checked = if probe.fallback_reason == Some("shared-image") {
                if let Some(saved) = old.upgrade() {
                    previous.verify(
                        previous.dimensions(WIDTH),
                        saved.as_bytes(0).ok_or("旧共享帧缺少像素")?,
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
                ensure(old_expired, "重填移出 buffer 后旧 Arc 仍然存活")?;
            }
            report.record("refill-processed-presented", serde_json::json!({
                "round":round,"step":step,"force_borrowed":force_borrowed,"eligible":eligible,
                "width":oracle.dimensions(WIDTH).0,"height":oracle.dimensions(WIDTH).1,
                "all_pixels_checked":u64::from(WIDTH)*u64::from(HEIGHT),"all_alpha_checked":true,
                "original_all_pixels_unchanged":true,"expected_image_materialized":false,
                "workspace_save_busy_false_at_atomic_perform":true,
                "processing_through_presentation_wall_ms":elapsed_ms,"process_cpu_ms":cpu_ms,
                "probe":probe,"lifecycle":next_life,"framebuffer_samples":samples,"old_weak_expired":old_expired,
                "shared_old_all_pixels_checked_while_owner_alive":shared_old_all_pixels_checked,
                "shared_owner_identity_inspected":false,
            }))?;
            previous = oracle;
            life = next_life;
            refill_counter = probe.original_refills;
            old_frames.push(old);
        }
        let current = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
        close(pin, original, cx).await?;
        wait_hwnd_destroyed(handle, cx).await?;
        ensure(
            current.upgrade().is_none() && old_frames.iter().all(|old| old.upgrade().is_none()),
            "重填关闭后当前/旧帧仍被保留",
        )?;
        ensure(
            crate::long_capture_runtime::active_workers() == 0
                && crate::long_capture_pipeline::active_capture_threads() == 0,
            "重填关闭后仍有消费者/采集线程",
        )?;
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        let memory = cpu_memory()?;
        let private = memory["private_bytes"]
            .as_u64()
            .ok_or("重填关闭内存缺少 private_bytes")?;
        closed.push(private);
        report.record("refill-round-closed", serde_json::json!({
            "round":round,"private_bytes_after_500ms":private,"source_weak_expired":true,"current_weak_expired":true,
            "entity_removed":true,"owned_hwnd_destroyed":true,"capture_workers":0,"consumer_sessions":0,
            "actual_unique_refill_hits":round_reuses,"old_frame_weaks_expired":old_frames.len(),
        }))?;
        ensure(
            force_borrowed || round_reuses == 9,
            "本轮尚未覆盖全部九种实际独占重填路径，保留真实共享来源待调查",
        )?;
    }
    let growth = closed
        .iter()
        .copied()
        .max()
        .ok_or("重填没有关闭内存样本")?
        .saturating_sub(closed[0]);
    report.record("refill-close-memory-gate", serde_json::json!({"private_bytes":closed,"growth_bytes":growth,"limit_bytes":CLOSE_LIMIT,"ok":growth<=CLOSE_LIMIT}))?;
    ensure(
        growth <= CLOSE_LIMIT,
        "三轮重填贴图关闭内存增长超过原 16MiB 门",
    )?;
    ensure(eligible_count == 27, "重填 A/B eligible 路径计数错误")?;
    if force_borrowed {
        ensure(
            forced_count == 27 && unique_count == 0,
            "重填强制借用对照没有 27 次 QAForced/零复用",
        )?;
    } else {
        ensure(
            unique_count == 27
                && shared_count == 0
                && REFILL_KINDS
                    .iter()
                    .all(|kind| unique_by_kind.get(kind) == Some(&3)),
            "三轮所有重填种类尚未实际命中独占 Vec",
        )?;
    }
    report.groups.push(serde_json::json!({
        "name":if force_borrowed {"three-rounds-32000px-pin-refill-borrowed"} else {"three-rounds-32000px-pin-refill-reuse"},
        "ok":true,"rounds":3,"cycles":3,"steps_per_round":11,"processed_frames":33,
        "eligible_refill_steps":eligible_count,"unique_refill_hits":unique_count,"qa_forced_borrowed_hits":forced_count,
        "shared_fallback_hits":shared_count,"unique_by_kind":unique_by_kind,"actual_absolute_turns_checked":[0,1,3],
        "source_dimensions":[WIDTH,HEIGHT],"close_private_bytes":closed,"close_private_growth_bytes":growth,"close_private_limit_bytes":CLOSE_LIMIT,
        "same_binary_ab":true,"borrowed_is_old_binary":false,"all_bgra_pixels_checked":true,"all_alpha_checked":true,
        "original_all_pixels_unchanged":true,"expected_image_materialized":false,"gpu_all_pixels_checked":false,"gpu_sampled_stable_blocks_per_step":2,
        "clipboard_untouched":true,"desktop_pixels_read":false,"global_input_sent":false,"shared_png_inflight_checked":false,
        "limitations":["相同最终二进制的 QA 强制借用控制，不是旧版本实测", "GPU 只读两个独立稳定块，全部 BGRA/alpha 由 CPU 行 oracle 检查", "共享 PNG 另在冷工作进程验收，不预热本组", "有限三轮不证明长期无泄漏或真实浏览器/采集硬件表现"],
    }));
    report.save(None, false)
}

async fn block_shared_png(
    pin: WindowHandle<PinView>,
    path: PathBuf,
    cx: &mut AsyncApp,
) -> Result<(EncodingJob, Weak<gpui::RenderImage>, usize, usize)> {
    let (snapshot, old, pointer, image_id) = pin.update(cx, |v, _, _| -> Result<_> {
        let snapshot = snapshot_pixels(&v.image, v.image_size, true, || {
            Err("Shared 重填专项不应调用输出回退".into())
        })?;
        ensure(
            matches!(&snapshot, SnapshotPixels::Shared { .. }),
            "重填生产快照工厂没有返回 Shared",
        )?;
        let (size, pixels) = snapshot.pixels()?;
        let pointer = v.image.as_bytes(0).ok_or("当前共享图像没有像素")?.as_ptr() as usize;
        ensure(
            size == v.image_size && pixels.as_ptr() as usize == pointer,
            "重填共享快照没有直接借用当前图",
        )?;
        Ok((snapshot, Arc::downgrade(&v.image), pointer, v.image.id.0))
    })??;
    let job = EncodingJob::start(snapshot, path.clone());
    let end = Instant::now() + WAIT;
    while !job.entered()? {
        ensure(
            Instant::now() < end,
            "重填共享 PNG 编码未到实际 header 后阻塞点",
        )?;
        tick(cx).await;
    }
    let mut header = [0; 33];
    File::open(&path)?.read_exact(&mut header)?;
    ensure(
        &header[..8] == b"\x89PNG\r\n\x1a\n" && &header[12..16] == b"IHDR",
        "重填共享 PNG 阻塞前没有写出真实 header",
    )?;
    Ok((job, old, pointer, image_id))
}

fn verify_saved(old: &Weak<gpui::RenderImage>, oracle: Oracle, width: u32) -> Result<()> {
    let saved = old.upgrade().ok_or("在途共享 PNG 丢失原始旧帧")?;
    oracle.verify(
        oracle.dimensions(width),
        saved.as_bytes(0).ok_or("共享旧帧没有像素")?,
        width,
    )
}

async fn decode_png(path: PathBuf, oracle: Oracle, cx: &mut AsyncApp) -> Result<()> {
    let decoded = cx
        .background_executor()
        .spawn(async move { crate::export::read_image(&path).map_err(|error| error.to_string()) })
        .await
        .map_err(|error| error.to_string())?;
    oracle.verify((decoded.width, decoded.height), &decoded.bgra, 64)?;
    drop(decoded);
    Ok(())
}

pub(super) async fn verify_shared(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let pin = open(64, false, cx).await?;
    let (original, handle) = pin.update(cx, |v, w, _| {
        (Arc::downgrade(&v.original), hwnd(w).0 as usize)
    })?;
    let _ = perform_after_save(pin, PinAction::Brightness(10), cx).await?;
    ready(pin, cx).await?;
    let bright = Oracle {
        brightness: 10,
        ..Default::default()
    };
    verify_pixels(pin, bright, 64, cx)?;
    let path1 = report.root.join("refill-shared-brightness-10.png");
    let (mut first, first_old, first_pointer, first_id) =
        block_shared_png(pin, path1.clone(), cx).await?;
    let _ = perform_after_save(pin, PinAction::Brightness(10), cx).await?;
    ready(pin, cx).await?;
    let brighter = Oracle {
        brightness: 20,
        ..Default::default()
    };
    verify_pixels(pin, brighter, 64, cx)?;
    let color_probe = pin.update(cx, |v, _, _| v.processing_probe())?;
    ensure(
        color_probe.completed
            && !color_probe.failed
            && !color_probe.reused
            && !color_probe.image_unavailable
            && color_probe.choice == Some("borrowed-original")
            && color_probe.fallback_reason == Some("shared-image")
            && color_probe.old_image_id == first_id
            && color_probe.old_cpu_pointer == first_pointer
            && color_probe.new_image_id != first_id
            && color_probe.new_cpu_pointer != first_pointer,
        "在途共享 PNG 的色彩重填没有安全 Shared fallback",
    )?;
    verify_saved(&first_old, bright, 64)?;
    report.record("refill-shared-color-presented", serde_json::json!({"probe":color_probe,"old_snapshot_all_bgra_unchanged":true,"original_all_bgra_unchanged":true,"header_written_before_action":true}))?;

    // 第二次工厂冻结当前新图；首份仍在真实编码中，不能误把旧图的引用当作当前图共享。
    let path2 = report.root.join("refill-shared-brightness-20.png");
    let (mut second, second_old, second_pointer, second_id) =
        block_shared_png(pin, path2.clone(), cx).await?;
    let _ = perform_after_save(pin, PinAction::Rotate(1), cx).await?;
    ready(pin, cx).await?;
    verify_pixels(
        pin,
        Oracle {
            turns: 1,
            ..brighter
        },
        64,
        cx,
    )?;
    let quarter_probe = pin.update(cx, |v, _, _| v.processing_probe())?;
    ensure(
        quarter_probe.completed
            && !quarter_probe.failed
            && !quarter_probe.reused
            && !quarter_probe.image_unavailable
            && quarter_probe.choice == Some("borrowed-original")
            && quarter_probe.fallback_reason == Some("shared-image")
            && quarter_probe.old_image_id == second_id
            && quarter_probe.old_cpu_pointer == second_pointer
            && quarter_probe.new_image_id != second_id
            && quarter_probe.new_cpu_pointer != second_pointer,
        "在途当前 Shared PNG 的 quarter 重填没有安全 fallback",
    )?;
    verify_saved(&first_old, bright, 64)?;
    verify_saved(&second_old, brighter, 64)?;
    report.record("refill-shared-quarter-presented", serde_json::json!({"probe":quarter_probe,"old_snapshots_all_bgra_unchanged":true,"original_all_bgra_unchanged":true,"header_written_before_action":true,"actual_output_dimensions":[HEIGHT,64]}))?;
    first.finish(cx).await?;
    second.finish(cx).await?;
    let end = Instant::now() + WAIT;
    while first_old.upgrade().is_some() || second_old.upgrade().is_some() {
        ensure(Instant::now() < end, "实际 PNG join 后旧共享帧尚未正常释放")?;
        tick(cx).await;
    }
    decode_png(path1, bright, cx).await?;
    decode_png(path2, brighter, cx).await?;
    let current = pin.update(cx, |v, _, _| Arc::downgrade(&v.image))?;
    close(pin, original, cx).await?;
    wait_hwnd_destroyed(handle, cx).await?;
    ensure(current.upgrade().is_none(), "共享重填关闭后当前帧仍保留")?;
    ensure(
        crate::long_capture_runtime::active_workers() == 0
            && crate::long_capture_pipeline::active_capture_threads() == 0,
        "共享重填关闭后仍有消费者/采集线程",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    report.record("refill-shared-closed", serde_json::json!({
        "source_dimensions":[64,HEIGHT],"png_count":2,"snapshot_weak_expired":true,"source_weak_expired":true,
        "current_weak_expired":true,"entity_removed":true,"owned_hwnd_destroyed":true,"capture_workers":0,"consumer_sessions":0,
        "encoder_threads_joined":2,"decoded_png_all_bgra_correct":true,"workspace_tick_and_manifest_commit_tested":false,
    }))?;
    report.groups.push(serde_json::json!({
        "name":"shared-png-pin-refill-buffer","ok":true,"source_dimensions":[64,HEIGHT],"processed_frames":3,
        "real_snapshot_factory_shared":true,"real_streaming_encoder":true,"real_encoder_headers_before_actions":true,
        "shared_color_fallback_checked":true,"shared_quarter_fallback_checked":true,"shared_png_count":2,
        "old_snapshots_all_bgra_unchanged":true,"new_image_all_bgra_correct":true,"original_all_pixels_unchanged":true,
        "all_alpha_checked":true,"decoded_png_all_bgra_correct":true,"encoder_threads_joined":2,
        "snapshot_weaks_expired":true,"source_weak_expired":true,"current_weak_expired":true,
        "owned_hwnd_destroyed":true,"entity_removed":true,"capture_workers":0,"consumer_sessions":0,
        "color_probe":color_probe,"quarter_probe":quarter_probe,"workspace_tick_and_manifest_commit_tested":false,
        "clipboard_untouched":true,"desktop_pixels_read":false,"global_input_sent":false,
        "limitations":["实际生产 Shared 快照工厂与流式编码器，未覆盖真实工作区 tick/manifest 提交", "两个约7.8MiB PNG 依次解码并释放，未混入960×32000冷对照进程", "本组仅共享保护，不能冒称三轮内存/完整GPU全图验收"],
    }));
    report.save(None, false)
}
