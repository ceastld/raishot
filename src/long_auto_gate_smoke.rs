//! 同一真实抓屏门撤约：证明生产 Auto 的综合安全停止，不冒充独立 post 分支证明。
use super::{
    acknowledged, committed, ensure, expect_image, fixture, long_capture_pipeline,
    long_capture_runtime, observe_no_more, same_segments, start_auto, wait, AutoDirection,
    AutoPhase, LongPhase, LongSession, Qa, ScrollAxis, Session, WAIT,
};
use crate::{long_capture_runtime::FrameGate, Result};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::IsWindow};

const REVOKED: &str = "QA capture gate lease revoked";

#[derive(Default)]
struct LeaseGate {
    revoked: AtomicBool,
    checks: AtomicU64,
    rejected: AtomicU64,
}

pub(super) fn verify(qa: &mut Qa) -> Result<serde_json::Value> {
    let mut target = qa.fixture(ScrollAxis::Vertical, (0, 0))?;
    let owned_gate = target.capture_gate()?;
    let lease = Arc::new(LeaseGate::default());
    let gate_lease = lease.clone();
    let gate: FrameGate = Arc::new(move || {
        gate_lease.checks.fetch_add(1, Ordering::AcqRel);
        // 不绕开原 HWND/全 ROI 的真实归属检查；撤约门与生产所有采集入口共用。
        owned_gate()?;
        if gate_lease.revoked.load(Ordering::Acquire) {
            gate_lease.rejected.fetch_add(1, Ordering::AcqRel);
            return Err(REVOKED.into());
        }
        Ok(())
    });
    let mut options = fixture::options(ScrollAxis::Vertical, (0, 0));
    options.poll_interval = Duration::from_millis(16);
    let session = LongSession::start_guarded(target.region, options, gate)?;
    let probe = session.worker_probe();
    let mut guard = Session {
        session,
        probe,
        closed: false,
    };
    wait(&guard.session, "撤约专项真实 DXGI 初帧", |m| {
        acknowledged(m) && m.phase == LongPhase::Running && m.result_available
    })?;
    let baseline = committed(&guard.session)?;
    let weaks: Vec<_> = baseline
        .segments
        .iter()
        .map(|s| Arc::downgrade(&s.image))
        .collect();
    let initial = expect_image(
        &baseline,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        0,
        "同抓屏门撤约初帧独立 oracle",
    )?;
    // 半格不移动且 1× 间隔为100ms；只由生产 Auto 投递，不由 QA 发滚轮。
    start_auto(&mut guard.session, AutoDirection::Forward)?;
    wait(&guard.session, "撤约前真实第一半格收件", |m| {
        acknowledged(m)
            && m.phase == LongPhase::Running
            && m.auto.active
            && target.wheel_count() == 1
            && target.wheel_events().len() == 1
    })?;
    let first_events = target.wheel_events();
    ensure(
        first_events.len() == 1
            && first_events[0].delta == -60
            && first_events[0].completed_notches == 0
            && target.origin() == (fixture::ORIGIN, fixture::ORIGIN),
        "撤约前没有真实收到未移动的生产半格 WM_MOUSEWHEEL",
    )?;
    let before = guard.session.snapshot();
    let before_pipeline = guard.session.pipeline_probe();
    ensure(
        before_pipeline.last_post_fence.is_some(),
        "真实收件缺少生产 post fence",
    )?;
    let rejections_before = lease.rejected.load(Ordering::Acquire);
    ensure(rejections_before == 0, "未撤约的门已拒绝真实采集")?;
    lease.revoked.store(true, Ordering::Release);
    // 此后 QA 不主动调用 wrapper；rejected 只来自真实生产采集/提交/投递入口。
    let deadline = Instant::now() + WAIT;
    let terminal = loop {
        let m = guard.session.snapshot();
        let p = guard.session.pipeline_probe();
        if acknowledged(&m)
            && !m.auto.active
            && m.auto.phase == AutoPhase::Blocked
            && matches!(m.phase, LongPhase::Paused | LongPhase::Failed)
            && !m.busy
            && p.producer_quiesced
            && !p.capture_in_flight
            && !p.consumer_in_flight
            && p.queue_depth == 0
            && p.reserved_raw_bytes == 0
            && p.drain_fence.is_none()
        {
            ensure(
                m.auto.status.contains(REVOKED) && m.status.contains(REVOKED),
                "安全停止没有给出同抓屏门的真实撤约错误",
            )?;
            break m;
        }
        ensure(
            !m.worker_exited && Instant::now() < deadline,
            &format!("抓屏门撤约没有正常收束：{m:?}；{p:?}"),
        )?;
        thread::sleep(Duration::from_millis(5));
    };
    ensure(
        lease.rejected.load(Ordering::Acquire) > rejections_before,
        "生产入口未实际遇到撤约门",
    )?;
    ensure(
        unsafe { IsWindow(Some(HWND(target.native() as *mut _))) }.as_bool(),
        "抓屏门撤约时原目标 HWND 应仍然有效",
    )?;
    (target.capture_gate()?)()?;
    let before_quiet = guard.session.pipeline_probe();
    let count = observe_no_more(&target)?;
    ensure(count == 1, "撤约后发生额外的实际滚轮消息")?;
    let stable = guard.session.pipeline_probe();
    let quiet_meta = guard.session.snapshot();
    ensure(
        stable.last_issued_sequence == before_quiet.last_issued_sequence
            && stable.processed_sequence == before_quiet.processed_sequence
            && stable.producer_quiesced
            && !stable.capture_in_flight
            && !stable.consumer_in_flight
            && stable.queue_depth == 0
            && stable.reserved_raw_bytes == 0
            && !quiet_meta.auto.active,
        "安静观察期仍发起或处理采集",
    )?;
    let saved = guard
        .session
        .stitch_snapshot()
        .ok_or("撤约安全停止后缺少生产可输出快照")?;
    ensure(
        same_segments(&baseline, &saved),
        "撤销抓屏门污染了可信输出条带 Arc",
    )?;
    fixture::same_image(
        &fixture::stitched_image(&saved)?,
        &initial,
        "同抓屏门撤约保留的完整输出像素",
    )?;
    ensure(
        quiet_meta.auto.steps == 1
            && terminal.auto.steps == quiet_meta.auto.steps
            && stable.last_post_fence == before_pipeline.last_post_fence,
        "撤约后生产 post 计数或 fence 继续推进",
    )?;
    qa.observe_capture(&target, &guard.session)?;
    let denied = lease.rejected.load(Ordering::Acquire);
    let checks = lease.checks.load(Ordering::Acquire);
    let trigger = if terminal.phase == LongPhase::Failed {
        "capture-gate-production-failure"
    } else {
        "capture-gate-auto-input-blocked"
    };
    let closed = guard.close()?;
    drop(saved);
    drop(baseline);
    drop(initial);
    drop(guard);
    ensure(
        weaks.iter().all(|w| w.upgrade().is_none()),
        "撤约专项正常退出后仍持有可信源图",
    )?;
    let handle = target.native();
    target.close()?;
    ensure(
        !unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool()
            && long_capture_runtime::active_workers() == 0
            && long_capture_pipeline::active_capture_threads() == 0,
        "撤约专项遗留自有窗口或采集线程",
    )?;
    Ok(serde_json::json!({
        "hwnd":handle,"actual_wheel_count":count,"events":first_events,
        "fixture_origin":[fixture::ORIGIN,fixture::ORIGIN],
        "same_capture_gate_revoked":true,"gate_checks":checks,"gate_rejections":denied,
        "explicit_qa_rejections":0,"gate_rejections_before_revoke":rejections_before,
        "production_gate_rejection_observed":denied>rejections_before,
        "trigger":trigger,"capture_phase":format!("{:?}",terminal.phase),
        "auto_phase":format!("{:?}",terminal.auto.phase),"auto_status":terminal.auto.status,
        "status":terminal.status,"before":format!("{before:?}"),
        "before_pipeline":before_pipeline,"terminal_pipeline":stable,
        "post_guard_branch_isolated":false,"post_guard_branch_evidence":"separate-core-test",
        "target_remained_alive_at_rejection":true,"underlying_owned_gate_still_valid":true,
        "baseline_same_arc":true,"all_output_pixels_unchanged":true,
        "source_weaks_expired":true,"owned_hwnd_destroyed":true,
        "quiet_observed_ms":super::QUIET_CHECK.as_millis(),"close":closed,
        "limitations":["综合生产安全停止；producer可能先Failed，非独立post分支原生证明",
        "真实自有Win32/DXGI，未使用Chrome/CDP或验证DOM路由"]
    }))
}
