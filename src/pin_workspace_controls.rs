//! 单张贴图控制的纯身份、期限和幂等规则；不持窗口或文件句柄。

pub(crate) const MAX_REQUEST_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum InteractionAction {
    Unlock = 1,
    ClearClickThrough = 2,
    Lock = 3,
    EnableClickThrough = 4,
    /// 不改状态：贴图置前并显示提示气泡，帮用户在屏幕上找到它。
    Locate = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) scope: u64,
    pub(crate) record_key: u64,
    pub(crate) generation: u64,
    pub(crate) hwnd: usize,
    pub(crate) pid: u32,
    pub(crate) thread: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveControl {
    pub(crate) identity: Identity,
    pub(crate) locked: bool,
    pub(crate) click_through: bool,
    pub(crate) busy: bool,
    pub(crate) closing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) identity: Identity,
    pub(crate) ticket: u64,
    pub(crate) action: InteractionAction,
    pub(crate) deadline_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Outcome {
    Success = 1,
    Stale = 2,
    Busy = 3,
    Expired = 4,
    Failed = 5,
    Closing = 6,
}

pub(crate) fn parse_action(tag: usize) -> Option<InteractionAction> {
    match tag {
        1 => Some(InteractionAction::Unlock),
        2 => Some(InteractionAction::ClearClickThrough),
        3 => Some(InteractionAction::Lock),
        4 => Some(InteractionAction::EnableClickThrough),
        5 => Some(InteractionAction::Locate),
        _ => None,
    }
}

pub(crate) fn parse_outcome(tag: usize) -> Option<Outcome> {
    match tag {
        1 => Some(Outcome::Success),
        2 => Some(Outcome::Stale),
        3 => Some(Outcome::Busy),
        4 => Some(Outcome::Expired),
        5 => Some(Outcome::Failed),
        6 => Some(Outcome::Closing),
        _ => None,
    }
}

/// ACK 尚未读走时也占有槽位；超过硬期限后才允许另一管理器替换。
pub(crate) fn slot_reserved(ticket: u64, deadline_ms: u64, now_ms: u64) -> bool {
    ticket != 0 && deadline_ms > now_ms && deadline_ms - now_ms <= MAX_REQUEST_MS
}

/// 只有当前请求、回执和读取者的非零票据全相同，读取者才可释放槽位。
pub(crate) fn ack_owns_slot(request_ticket: u64, ack_ticket: u64, read_ticket: u64) -> bool {
    request_ticket != 0 && request_ticket == ack_ticket && request_ticket == read_ticket
}

/// 相同记录重新恢复、HWND 复用和迟到票据都不能继承旧请求。
pub(crate) fn authorize(
    request: &Request,
    live: &LiveControl,
    now_ms: u64,
    last_ticket: u64,
) -> std::result::Result<(), Outcome> {
    let identity = request.identity;
    if identity != live.identity
        || identity.scope == 0
        || identity.record_key == 0
        || identity.generation == 0
        || identity.hwnd == 0
        || identity.pid == 0
        || identity.thread == 0
        || request.ticket == 0
        || request.ticket <= last_ticket
    {
        return Err(Outcome::Stale);
    }
    if live.closing {
        return Err(Outcome::Closing);
    }
    if live.busy {
        return Err(Outcome::Busy);
    }
    // 先排除过期，再相减检查未来期限，避免 now + MAX_REQUEST_MS 溢出。
    // 墙钟回拨超过请求窗口也拒绝，不能把旧请求延长到无界未来。
    if now_ms >= request.deadline_ms || request.deadline_ms - now_ms > MAX_REQUEST_MS {
        return Err(Outcome::Expired);
    }
    Ok(())
}

pub(crate) fn desired(
    action: InteractionAction,
    locked: bool,
    click_through: bool,
) -> (bool, bool) {
    match action {
        InteractionAction::Unlock => (false, click_through),
        InteractionAction::ClearClickThrough => (locked, false),
        InteractionAction::Lock => (true, click_through),
        InteractionAction::EnableClickThrough => (locked, true),
        InteractionAction::Locate => (locked, click_through),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> LiveControl {
        LiveControl {
            identity: Identity {
                scope: 11,
                record_key: 22,
                generation: 33,
                hwnd: 44,
                pid: 55,
                thread: 66,
            },
            locked: true,
            click_through: true,
            busy: false,
            closing: false,
        }
    }

    fn request(live: &LiveControl) -> Request {
        Request {
            identity: live.identity,
            ticket: 8,
            action: InteractionAction::Unlock,
            deadline_ms: 1_100,
        }
    }

    #[test]
    fn pin_workspace_controls_rejects_every_changed_target_identity() {
        let live = live();
        let request = request(&live);
        assert_eq!(authorize(&request, &live, 1_000, 7), Ok(()));
        let changes: [fn(&mut Identity); 6] = [
            |identity| identity.scope += 1,
            |identity| identity.record_key += 1,
            |identity| identity.generation += 1,
            |identity| identity.hwnd += 1,
            |identity| identity.pid += 1,
            |identity| identity.thread += 1,
        ];
        for change in changes {
            let mut stale = request;
            change(&mut stale.identity);
            assert_eq!(authorize(&stale, &live, 1_000, 7), Err(Outcome::Stale));
            let mut restored = live;
            restored.identity = stale.identity;
            assert_eq!(
                authorize(&request, &restored, 1_000, 7),
                Err(Outcome::Stale)
            );
        }
    }

    #[test]
    fn pin_workspace_controls_rejects_matching_but_incomplete_identity() {
        let empty_fields: [fn(&mut Identity); 6] = [
            |identity| identity.scope = 0,
            |identity| identity.record_key = 0,
            |identity| identity.generation = 0,
            |identity| identity.hwnd = 0,
            |identity| identity.pid = 0,
            |identity| identity.thread = 0,
        ];
        for empty in empty_fields {
            let mut live = live();
            empty(&mut live.identity);
            assert_eq!(
                authorize(&request(&live), &live, 1_000, 7),
                Err(Outcome::Stale)
            );
        }
    }

    #[test]
    fn pin_workspace_controls_refuses_saving_and_closing_without_mutation() {
        let mut live = live();
        let request = request(&live);
        live.busy = true;
        let before = live;
        assert_eq!(authorize(&request, &live, 1_000, 7), Err(Outcome::Busy));
        assert_eq!(live, before);
        live.closing = true;
        let before = live;
        assert_eq!(authorize(&request, &live, 1_000, 7), Err(Outcome::Closing));
        assert_eq!(live, before);
        live.busy = false;
        assert_eq!(authorize(&request, &live, 1_000, 7), Err(Outcome::Closing));
    }

    #[test]
    fn pin_workspace_controls_rejects_zero_duplicate_and_late_tickets() {
        let live = live();
        let mut request = request(&live);
        request.ticket = 0;
        assert_eq!(authorize(&request, &live, 1_000, 0), Err(Outcome::Stale));
        request.ticket = 7;
        assert_eq!(authorize(&request, &live, 1_000, 7), Err(Outcome::Stale));
        request.ticket = 6;
        assert_eq!(authorize(&request, &live, 1_000, 7), Err(Outcome::Stale));
        request.ticket = 8;
        assert_eq!(authorize(&request, &live, 1_000, 7), Ok(()));
        assert_eq!(authorize(&request, &live, 1_000, 8), Err(Outcome::Stale));
        assert_eq!(
            authorize(&request, &live, 1_000, u64::MAX),
            Err(Outcome::Stale)
        );
    }

    #[test]
    fn pin_workspace_controls_bounds_expiry_future_and_clock_rollback() {
        let live = live();
        let mut request = request(&live);
        for deadline in [0, 999, 1_000, 1_000 + MAX_REQUEST_MS + 1] {
            request.deadline_ms = deadline;
            assert_eq!(authorize(&request, &live, 1_000, 7), Err(Outcome::Expired));
        }
        request.deadline_ms = 1_001;
        assert_eq!(authorize(&request, &live, 1_000, 7), Ok(()));
        request.deadline_ms = 1_000 + MAX_REQUEST_MS;
        assert_eq!(authorize(&request, &live, 1_000, 7), Ok(()));
        assert_eq!(authorize(&request, &live, 999, 7), Err(Outcome::Expired));
        request.deadline_ms = u64::MAX;
        assert_eq!(authorize(&request, &live, u64::MAX - 2_000, 7), Ok(()));
        assert_eq!(
            authorize(&request, &live, u64::MAX, 7),
            Err(Outcome::Expired)
        );
    }

    #[test]
    fn pin_workspace_controls_is_idempotent_and_keeps_other_setting() {
        for locked in [false, true] {
            for through in [false, true] {
                let unlocked = desired(InteractionAction::Unlock, locked, through);
                assert_eq!(unlocked, (false, through));
                assert_eq!(
                    desired(InteractionAction::Unlock, unlocked.0, unlocked.1),
                    unlocked
                );
                let interactive = desired(InteractionAction::ClearClickThrough, locked, through);
                assert_eq!(interactive, (locked, false));
                assert_eq!(
                    desired(
                        InteractionAction::ClearClickThrough,
                        interactive.0,
                        interactive.1
                    ),
                    interactive
                );
            }
        }
    }

    #[test]
    fn pin_workspace_controls_parses_only_defined_wire_tags() {
        for action in [
            InteractionAction::Unlock,
            InteractionAction::ClearClickThrough,
        ] {
            assert_eq!(parse_action(action as usize), Some(action));
        }
        for outcome in [
            Outcome::Success,
            Outcome::Stale,
            Outcome::Busy,
            Outcome::Expired,
            Outcome::Failed,
            Outcome::Closing,
        ] {
            assert_eq!(parse_outcome(outcome as usize), Some(outcome));
        }
        for tag in [0, 6, usize::MAX] {
            assert_eq!(parse_action(tag), None);
        }
        for tag in [0, 7, usize::MAX] {
            assert_eq!(parse_outcome(tag), None);
        }
    }

    #[test]
    fn pin_workspace_controls_unread_ack_reserves_slot_until_its_reader_releases_it() {
        let reader_a = 8;
        let reader_b = 9;
        let deadline = 1_000 + MAX_REQUEST_MS;
        let mut request_ticket = reader_a;
        assert!(slot_reserved(request_ticket, deadline, 1_000));
        // A 已应用并得到回执，但 A 尚未读取；B 不能占用该槽位。
        let ack_ticket = reader_a;
        assert!(slot_reserved(request_ticket, deadline, 1_001));
        assert!(!ack_owns_slot(request_ticket, ack_ticket, reader_b));
        assert!(slot_reserved(request_ticket, deadline, deadline - 1));
        assert!(ack_owns_slot(request_ticket, ack_ticket, reader_a));
        request_ticket = 0;
        assert!(!slot_reserved(request_ticket, deadline, 1_001));
        assert!(!ack_owns_slot(request_ticket, ack_ticket, reader_a));
        request_ticket = reader_b;
        assert!(slot_reserved(request_ticket, deadline, 1_001));

        assert!(!slot_reserved(0, deadline, 1_000));
        for invalid_deadline in [0, 999, 1_000, deadline + 1] {
            assert!(!slot_reserved(reader_a, invalid_deadline, 1_000));
        }
        assert!(slot_reserved(reader_a, 1_001, 1_000));
        assert!(slot_reserved(reader_a, u64::MAX, u64::MAX - MAX_REQUEST_MS));
        assert!(!slot_reserved(reader_a, u64::MAX, u64::MAX));
        assert!(!slot_reserved(reader_a, deadline, 999));
        for (request, ack, read) in [(0, 0, 0), (0, 8, 8), (8, 0, 8), (8, 8, 0)] {
            assert!(!ack_owns_slot(request, ack, read));
        }
    }

    #[test]
    fn pin_workspace_controls_expired_slot_replacement_rejects_a_late_ack_reader() {
        let reader_a = 8;
        let reader_b = 9;
        let mut request_ticket = reader_a;
        let mut ack_ticket = reader_a;
        let expired_deadline = 2_000;
        assert!(slot_reserved(request_ticket, expired_deadline, 1_999));
        assert!(!slot_reserved(request_ticket, expired_deadline, 2_000));

        // A 未及时读取，期限到了后 B 替换槽位；旧 ACK 或迟到 A 均无权清除 B。
        request_ticket = reader_b;
        let deadline_b = 2_000 + MAX_REQUEST_MS;
        assert!(slot_reserved(request_ticket, deadline_b, 2_000));
        assert!(!ack_owns_slot(request_ticket, ack_ticket, reader_a));
        assert!(!ack_owns_slot(request_ticket, ack_ticket, reader_b));
        ack_ticket = reader_b;
        assert!(!ack_owns_slot(request_ticket, ack_ticket, reader_a));
        assert!(slot_reserved(request_ticket, deadline_b, deadline_b - 1));
        assert!(ack_owns_slot(request_ticket, ack_ticket, reader_b));
        request_ticket = 0;
        assert!(!slot_reserved(request_ticket, deadline_b, deadline_b - 1));
    }

    #[test]
    fn pin_workspace_controls_actions_round_trip_and_toggle_one_field() {
        for action in [
            InteractionAction::Unlock,
            InteractionAction::ClearClickThrough,
            InteractionAction::Lock,
            InteractionAction::EnableClickThrough,
            InteractionAction::Locate,
        ] {
            assert_eq!(parse_action(action as usize), Some(action));
        }
        assert_eq!(parse_action(0), None);
        assert_eq!(parse_action(6), None);
        assert_eq!(desired(InteractionAction::Lock, false, true), (true, true));
        assert_eq!(
            desired(InteractionAction::EnableClickThrough, true, false),
            (true, true)
        );
        assert_eq!(
            desired(InteractionAction::Locate, true, false),
            (true, false)
        );
    }
}
