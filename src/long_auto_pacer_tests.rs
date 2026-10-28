use super::*;

fn at(start: Instant, ms: u64) -> Instant {
    start + Duration::from_millis(ms)
}
fn stop_phase(decision: Decision) -> AutoPhase {
    match decision {
        Decision::Stop { phase, .. } => phase,
        other => panic!("应当停止，实际为{other:?}"),
    }
}

fn strong_match() -> StitchMatchQuality {
    StitchMatchQuality {
        error: 0.5,
        matched_ratio: 0.98,
        unique: true,
        overlap_ratio: 0.9,
    }
}

struct AdaptiveRun {
    policy: AutoPolicy,
    now: Instant,
    poll: u64,
    position: i64,
}
impl AdaptiveRun {
    fn new() -> Self {
        let start = Instant::now();
        let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 7, start);
        assert_eq!(policy.snapshot(String::new()).speed, 1.);
        policy.observe(7, 1, UpdateKind::Added, 0, 0, start);
        assert_eq!(
            policy.observe(7, 2, UpdateKind::Unchanged, 0, 0, at(start, 99)),
            Decision::Wait
        );
        assert_eq!(
            policy.observe(7, 3, UpdateKind::Unchanged, 0, 0, at(start, 100)),
            Decision::Post
        );
        policy.sent(3, at(start, 100));
        Self {
            policy,
            now: at(start, 100),
            poll: 3,
            position: 0,
        }
    }
    fn frame(
        &mut self,
        ms: u64,
        kind: UpdateKind,
        delta: i32,
        quality: Option<StitchMatchQuality>,
    ) -> Decision {
        self.now += Duration::from_millis(ms);
        self.poll += 1;
        if kind != UpdateKind::Rejected {
            self.position += i64::from(delta);
        }
        self.policy
            .observe_match(7, self.poll, kind, delta, self.position, quality, self.now)
    }
    fn complete(&mut self, quality: Option<StitchMatchQuality>) {
        assert_eq!(
            self.frame(10, UpdateKind::Added, 16, quality),
            Decision::Wait
        );
        self.settle();
    }
    fn settle(&mut self) {
        for _ in 0..30 {
            match self.frame(10, UpdateKind::Unchanged, 0, None) {
                Decision::Post => {
                    self.policy.sent(self.poll, self.now);
                    return;
                }
                Decision::Wait => {}
                stop => panic!("unexpected stop: {stop:?}"),
            }
        }
        panic!("reliable response did not settle");
    }
    fn warm_up(&mut self) {
        for _ in 0..12 {
            self.complete(Some(strong_match()));
        }
        assert!(self.policy.snapshot(String::new()).speed > 1.);
    }
}

#[test]
fn long_auto_pacer_adaptive_starts_slow_and_accelerates_only_after_reliable_responses() {
    let mut run = AdaptiveRun::new();
    for _ in 0..40 {
        let before = run.policy.snapshot(String::new()).speed;
        run.complete(Some(strong_match()));
        let speed = run.policy.snapshot(String::new()).speed;
        assert!((before..=before + 0.5).contains(&speed));
        assert!((1.0..=10.0).contains(&speed));
    }
    assert_eq!(run.policy.snapshot(String::new()).speed, 10.);
    // 即使最高速度，两个未确认半格也只能等待，不能盲目补发第三个。
    assert_eq!(
        run.frame(20, UpdateKind::Unchanged, 0, None),
        Decision::Post
    );
    run.policy.sent(run.poll, run.now);
    assert_eq!(
        run.frame(20, UpdateKind::Unchanged, 0, None),
        Decision::Wait
    );
    assert_eq!(
        run.policy.idle(7, run.now + Duration::from_millis(50)),
        Decision::Wait
    );
}

#[test]
fn long_auto_pacer_adaptive_weak_matches_reduce_speed_and_recover_gradually() {
    for quality in [
        StitchMatchQuality {
            error: 5.,
            ..strong_match()
        },
        StitchMatchQuality {
            matched_ratio: 0.7,
            ..strong_match()
        },
        StitchMatchQuality {
            overlap_ratio: 0.3,
            ..strong_match()
        },
        StitchMatchQuality {
            unique: false,
            ..strong_match()
        },
        StitchMatchQuality {
            error: f64::NAN,
            ..strong_match()
        },
    ] {
        let mut run = AdaptiveRun::new();
        run.warm_up();
        let fast = run.policy.snapshot(String::new()).speed;
        assert_eq!(
            run.frame(10, UpdateKind::Added, 16, Some(quality)),
            Decision::Wait
        );
        let slow = run.policy.snapshot(String::new()).speed;
        assert!(slow < fast && slow >= 1.);
        run.settle();
        assert_eq!(run.policy.snapshot(String::new()).speed, slow);
        run.complete(Some(strong_match()));
        assert_eq!(
            run.policy.snapshot(String::new()).speed,
            slow,
            "一次恢复成功不能直接回到高速"
        );
        for _ in 0..4 {
            run.complete(Some(strong_match()));
        }
        assert!(run.policy.snapshot(String::new()).speed > slow);
    }
}

#[test]
fn long_auto_pacer_adaptive_rejection_and_slow_response_back_off_without_bypassing_guards() {
    let mut run = AdaptiveRun::new();
    run.warm_up();
    let fast = run.policy.snapshot(String::new()).speed;
    assert_eq!(
        run.frame(10, UpdateKind::Rejected, 16, Some(strong_match())),
        Decision::Wait
    );
    let slow = run.policy.snapshot(String::new()).speed;
    assert!(slow < fast);
    assert_eq!(
        run.frame(20, UpdateKind::Rejected, 16, Some(strong_match())),
        Decision::Wait
    );
    assert_eq!(
        run.policy.snapshot(String::new()).speed,
        slow,
        "同一响应只回退一次"
    );
    assert_eq!(
        stop_phase(run.policy.idle(7, run.now + Duration::from_millis(480))),
        AutoPhase::Unstable
    );

    let mut delayed = AdaptiveRun::new();
    delayed.warm_up();
    let fast = delayed.policy.snapshot(String::new()).speed;
    assert_eq!(
        delayed.frame(250, UpdateKind::Added, 16, Some(strong_match())),
        Decision::Wait
    );
    assert!(delayed.policy.snapshot(String::new()).speed < fast);
}

#[test]
fn long_auto_pacer_adaptive_ignores_old_frames_and_never_rewards_stationary_or_missing_quality() {
    let mut run = AdaptiveRun::new();
    for _ in 0..8 {
        run.complete(None);
    }
    assert_eq!(run.policy.snapshot(String::new()).speed, 1.);
    let steps = run.policy.snapshot(String::new()).steps;
    assert_eq!(
        run.policy.observe_match(
            7,
            run.poll,
            UpdateKind::Added,
            16,
            run.position + 16,
            Some(strong_match()),
            run.now
        ),
        Decision::Wait
    );
    assert_eq!(run.policy.snapshot(String::new()).steps, steps);
    assert_eq!(run.policy.snapshot(String::new()).speed, 1.);
    assert_eq!(
        run.frame(100, UpdateKind::Unchanged, 0, Some(strong_match())),
        Decision::Post
    );
    run.policy.sent(run.poll, run.now);
    assert_eq!(
        run.frame(100, UpdateKind::Unchanged, 0, Some(strong_match())),
        Decision::Wait
    );
    assert_eq!(run.policy.snapshot(String::new()).speed, 1.);
}

#[test]
fn long_auto_pacer_adaptive_smooth_response_keeps_weakest_evidence_and_only_rewards_once() {
    let mut run = AdaptiveRun::new();
    run.complete(Some(strong_match()));
    // 再一次强响应本可提速，但同一平滑动作内出现中等证据后不能以末帧覆盖。
    for quality in [
        strong_match(),
        StitchMatchQuality {
            error: 3.,
            ..strong_match()
        },
        strong_match(),
    ] {
        assert_eq!(
            run.frame(10, UpdateKind::Added, 8, Some(quality)),
            Decision::Wait
        );
        assert_eq!(run.policy.snapshot(String::new()).speed, 1.);
    }
    run.settle();
    assert_eq!(run.policy.snapshot(String::new()).speed, 1.);
    run.complete(Some(strong_match()));
    assert_eq!(run.policy.snapshot(String::new()).speed, 1.);
    run.complete(Some(strong_match()));
    assert_eq!(run.policy.snapshot(String::new()).speed, 1.5);

    run.warm_up();
    let fast = run.policy.snapshot(String::new()).speed;
    // 已有位移证据仍须等到静止帧；静止迟到同样说明页面吞吐不足。
    assert_eq!(
        run.frame(10, UpdateKind::Added, 8, Some(strong_match())),
        Decision::Wait
    );
    assert_eq!(
        run.frame(150, UpdateKind::Unchanged, 0, None),
        Decision::Post
    );
    assert!(run.policy.snapshot(String::new()).speed < fast);
}

#[test]
fn long_auto_pacer_adaptive_whole_notch_response_can_accelerate_without_extra_unconfirmed_steps() {
    let mut run = AdaptiveRun::new();
    for _ in 0..8 {
        // 目标只响应整格：第一个半格没动，第二个半格才形成真实匹配。
        for _ in 0..20 {
            if run.frame(10, UpdateKind::Unchanged, 0, None) == Decision::Post {
                run.policy.sent(run.poll, run.now);
                break;
            }
        }
        assert_eq!(run.policy.unacknowledged_delta, 120);
        assert_eq!(
            run.frame(10, UpdateKind::Unchanged, 0, None),
            Decision::Wait
        );
        run.complete(Some(strong_match()));
    }
    assert!(run.policy.snapshot(String::new()).speed > 1.);
}

#[test]
fn long_auto_pacer_whole_notch_only_target_receives_two_halves_then_requires_real_motion() {
    let start = Instant::now();
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 1, start);
    policy.observe(1, 1, UpdateKind::Added, 0, 0, start);
    let mut target_accumulated = 0;
    let mut target_position = 0;
    // 自有目标的独立合同：累计120才移动48px；第一条半格不能被误判为失效。
    for (poll, ms) in [(2, 100), (3, 200)] {
        assert_eq!(
            policy.observe(1, poll, UpdateKind::Unchanged, 0, 0, at(start, ms)),
            Decision::Post
        );
        target_accumulated += 60;
        if target_accumulated == 120 {
            target_position += 48;
            target_accumulated = 0;
        }
        policy.sent(poll, at(start, ms));
    }
    assert_eq!(target_position, 48);
    assert_eq!(target_accumulated, 0);
    assert_eq!(
        policy.observe(1, 4, UpdateKind::Unchanged, 0, 0, at(start, 300)),
        Decision::Wait,
        "目标实际已经滚动但帧尚未确认，不准按滚轮猜确认"
    );
    assert_eq!(
        policy.observe(
            1,
            5,
            UpdateKind::Added,
            target_position,
            i64::from(target_position),
            at(start, 320)
        ),
        Decision::Wait
    );
    assert_eq!(
        policy.observe(1, 6, UpdateKind::Unchanged, 0, 48, at(start, 349)),
        Decision::Wait
    );
    assert_eq!(
        policy.observe(1, 7, UpdateKind::Unchanged, 0, 48, at(start, 350)),
        Decision::Post
    );
    policy.sent(7, at(start, 350));
    assert_eq!(
        policy.observe(1, 8, UpdateKind::Unchanged, 0, 48, at(start, 450)),
        Decision::Post,
        "真实位移确认后，新的累计周期又允许两个半格"
    );
    policy.sent(8, at(start, 450));
    assert_eq!(
        policy.observe(1, 9, UpdateKind::Unchanged, 0, 48, at(start, 550)),
        Decision::Wait
    );
    assert_eq!(policy.snapshot(String::new()).steps, 4);
}

#[test]
fn long_auto_pacer_smooth_motion_waits_for_quiet_without_old_three_second_deadline() {
    let start = Instant::now();
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 1, start);
    policy.observe(1, 1, UpdateKind::Added, 0, 0, start);
    assert_eq!(
        policy.observe(1, 2, UpdateKind::Unchanged, 0, 0, at(start, 100)),
        Decision::Post
    );
    policy.sent(2, at(start, 100));
    for step in 1..=134 {
        assert_eq!(
            policy.observe(
                1,
                step + 2,
                UpdateKind::Added,
                8,
                step as i64 * 8,
                at(start, 100 + step * 30)
            ),
            Decision::Wait,
            "可信平滑运动未停止时不追发，但也不按旧3秒总时限中断"
        );
    }
    assert_eq!(
        policy.observe(1, 137, UpdateKind::Unchanged, 0, 1072, at(start, 4149)),
        Decision::Wait
    );
    assert_eq!(
        policy.observe(1, 138, UpdateKind::Unchanged, 0, 1072, at(start, 4150)),
        Decision::Post
    );
    assert_eq!(policy.snapshot(String::new()).steps, 1);
}

#[test]
fn long_auto_pacer_rejection_backpressure_has_exact_grace_and_recovers_on_accepted_frame() {
    let start = Instant::now();
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 1, start);
    policy.observe(1, 1, UpdateKind::Added, 0, 0, start);
    assert_eq!(
        policy.observe(1, 2, UpdateKind::Unchanged, 0, 0, at(start, 100)),
        Decision::Post
    );
    policy.sent(2, at(start, 100));
    assert_eq!(
        policy.observe(1, 3, UpdateKind::Rejected, 32, 32, at(start, 110)),
        Decision::Wait
    );
    assert_eq!(policy.idle(1, at(start, 609)), Decision::Wait);
    assert_eq!(
        policy.observe(1, 4, UpdateKind::Unchanged, 0, 0, at(start, 609)),
        Decision::Post,
        "参考认可的零位移恢复帧可以解除暂时失配"
    );
    policy.sent(4, at(start, 609));
    assert_eq!(
        policy.observe(1, 5, UpdateKind::Rejected, 0, 0, at(start, 3109)),
        Decision::Wait,
        "正在待匹配时先背压，不能因未ACK满120已到2500ms就抢先当作页面末尾"
    );
    assert_eq!(policy.idle(1, at(start, 3608)), Decision::Wait);
    assert_eq!(
        stop_phase(policy.idle(1, at(start, 3609))),
        AutoPhase::Unstable
    );
    assert_eq!(policy.snapshot(String::new()).steps, 2);
}

#[test]
fn long_auto_pacer_pre_post_and_duplicate_frames_cannot_ack_or_drive() {
    let start = Instant::now();
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 5, start);
    policy.observe(5, 1, UpdateKind::Added, 0, 0, start);
    assert_eq!(
        policy.observe(5, 2, UpdateKind::Unchanged, 0, 0, at(start, 100)),
        Decision::Post
    );
    // 原生post fence包含仍在读回/排队的序号，不只等于刚消费的poll。
    policy.sent(10, at(start, 100));
    for poll in [2, 3, 10] {
        assert_eq!(
            policy.observe(5, poll, UpdateKind::Added, 48, 48, at(start, 200)),
            Decision::Wait
        );
    }
    assert_eq!(
        policy.observe(5, 11, UpdateKind::Unchanged, 0, 0, at(start, 200)),
        Decision::Post
    );
    policy.sent(14, at(start, 200));
    for poll in [11, 12, 14] {
        assert_eq!(
            policy.observe(5, poll, UpdateKind::ViewportOnly, 48, 48, at(start, 250)),
            Decision::Wait
        );
    }
    assert_eq!(
        stop_phase(policy.idle(5, at(start, 2700))),
        AutoPhase::NoMotion,
        "旧许可里的假ACK不能解除累计120的等待"
    );
    assert_eq!(
        stop_phase(policy.observe(6, 15, UpdateKind::Unchanged, 0, 0, at(start, 2800))),
        AutoPhase::Off
    );
    assert_eq!(policy.snapshot(String::new()).steps, 2);
}

#[test]
fn long_auto_pacer_fresh_zero_delta_confirms_position_committed_by_old_permit() {
    let start = Instant::now();
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 9, start);
    assert_eq!(
        policy.observe(9, 1, UpdateKind::Added, 0, 0, start),
        Decision::Wait
    );
    assert_eq!(
        policy.observe(9, 2, UpdateKind::Unchanged, 0, 0, at(start, 100)),
        Decision::Post
    );
    policy.sent(10, at(start, 100));
    assert_eq!(
        policy.observe(9, 11, UpdateKind::Unchanged, 0, 0, at(start, 200)),
        Decision::Post
    );
    policy.sent(20, at(start, 200));
    // read20 在第二次post前已开始，却在post后读到16px位移并被拼接。
    // 它只改变外部可信位置，旧许可本身仍不能确认输入，也不能刷新policy位置。
    assert_eq!(
        policy.observe(9, 20, UpdateKind::Added, 16, 16, at(start, 240)),
        Decision::Wait
    );
    // 第一张post后新帧与已拼接帧完全相同，delta为0；真实位置仍可确认16px移动。
    assert_eq!(
        policy.observe(9, 21, UpdateKind::Unchanged, 0, 16, at(start, 250)),
        Decision::Wait,
        "位置确认当帧须重新等待30ms静止，不能直接追发"
    );
    assert_eq!(
        policy.observe(9, 22, UpdateKind::Unchanged, 0, 16, at(start, 279)),
        Decision::Wait
    );
    assert_eq!(
        policy.observe(9, 23, UpdateKind::Unchanged, 0, 16, at(start, 280)),
        Decision::Wait,
        "静置30ms已满足，但距上一发送只有80ms，仍需满足100ms倍率间隔"
    );
    assert_eq!(
        policy.observe(9, 24, UpdateKind::Unchanged, 0, 16, at(start, 300)),
        Decision::Post
    );
    policy.sent(24, at(start, 300));
    assert_eq!(
        policy.observe(9, 25, UpdateKind::Unchanged, 0, 16, at(start, 400)),
        Decision::Post,
        "可信位置确认应清掉前两个半格的累计，新周期还可发第二个半格"
    );
    policy.sent(25, at(start, 400));
    assert_eq!(
        policy.observe(9, 26, UpdateKind::Unchanged, 0, 16, at(start, 500)),
        Decision::Wait
    );
    assert_eq!(policy.snapshot(String::new()).steps, 4);
}

#[test]
fn long_auto_pacer_idle_timeouts_never_emit_input_and_full_notch_waits_2500_from_last_half() {
    let start = Instant::now();
    let baseline = AutoPolicy::new(AutoScrollOptions::default(), 1, start);
    assert_eq!(baseline.idle(1, at(start, 4999)), Decision::Wait);
    assert_eq!(
        stop_phase(baseline.idle(1, at(start, 5000))),
        AutoPhase::NoMotion
    );
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 1, start);
    policy.observe(1, 1, UpdateKind::Added, 0, 0, start);
    for (poll, ms) in [(2, 100), (3, 200)] {
        assert_eq!(
            policy.observe(1, poll, UpdateKind::Unchanged, 0, 0, at(start, ms)),
            Decision::Post
        );
        policy.sent(poll, at(start, ms));
    }
    assert_eq!(policy.idle(1, at(start, 2699)), Decision::Wait);
    assert_eq!(
        stop_phase(policy.idle(1, at(start, 2700))),
        AutoPhase::NoMotion
    );
    assert_eq!(policy.snapshot(String::new()).steps, 2);
}

#[test]
fn long_auto_pacer_direction_limit_and_generation_preserve_guards() {
    let start = Instant::now();
    let mut reverse = AutoPolicy::new(
        AutoScrollOptions {
            direction: AutoDirection::Reverse,
            ..Default::default()
        },
        3,
        start,
    );
    assert_eq!(
        reverse.observe(3, 1, UpdateKind::ViewportOnly, -32, -32, start),
        Decision::Wait
    );
    assert_eq!(
        stop_phase(reverse.observe(3, 2, UpdateKind::Added, 32, 0, at(start, 100))),
        AutoPhase::Blocked
    );
    assert_eq!(
        stop_phase(reverse.observe(3, 3, UpdateKind::Limit, 0, 0, at(start, 200))),
        AutoPhase::Off
    );
    assert_eq!(stop_phase(reverse.idle(4, at(start, 210))), AutoPhase::Off);
    assert_eq!(reverse.snapshot(String::new()).steps, 0);

    // 零delta的新帧仍须检查绝对位置方向；端点跨度也不能通过相减溢出。
    let mut forward = AutoPolicy::new(AutoScrollOptions::default(), 1, start);
    assert_eq!(
        forward.observe(1, 1, UpdateKind::Unchanged, 0, i64::MAX, start),
        Decision::Wait
    );
    assert_eq!(
        stop_phase(forward.observe(1, 2, UpdateKind::Unchanged, 0, i64::MIN, at(start, 100))),
        AutoPhase::Blocked
    );
    let mut reverse_position = AutoPolicy::new(
        AutoScrollOptions {
            direction: AutoDirection::Reverse,
            ..Default::default()
        },
        1,
        start,
    );
    reverse_position.observe(1, 1, UpdateKind::Unchanged, 0, i64::MIN, start);
    assert_eq!(
        stop_phase(reverse_position.observe(
            1,
            2,
            UpdateKind::Unchanged,
            0,
            i64::MAX,
            at(start, 100)
        )),
        AutoPhase::Blocked
    );
}
