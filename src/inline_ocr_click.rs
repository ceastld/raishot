//! 原位 OCR 普通按钮的真实 Down/Click 单次意图；不依赖窗口、剪贴板或 OCR 推理。
use std::{ops::Range, sync::Arc};

/// 保存 GPUI 实际 MouseDown 的所有字段。坐标保留浮点 bits，不做整数近似。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DownProof {
    pub(crate) left: bool,
    pub(crate) position: [u32; 2],
    pub(crate) modifiers: u8,
    pub(crate) click_count: usize,
    pub(crate) first_mouse: bool,
}

/// 来源与意图都取自真正接收 Down 的已渲染控件，Click 不重新发明 Down。
#[derive(Clone)]
pub(crate) struct ClickFrame<T, P, K, S, A, C> {
    pub(crate) body: Arc<()>,
    pub(crate) body_version: (u64, u64),
    pub(crate) projection_epoch: Arc<()>,
    pub(crate) panel_epoch: Arc<()>,
    pub(crate) target: T,
    pub(crate) projection: P,
    pub(crate) selection_generation: u64,
    pub(crate) selection: Option<Range<usize>>,
    pub(crate) cursor: Option<K>,
    pub(crate) color: Option<u32>,
    pub(crate) style: S,
    pub(crate) action: A,
    pub(crate) context: C,
}

impl<T: PartialEq, P: PartialEq, K: PartialEq, S: PartialEq, A: PartialEq, C: PartialEq> PartialEq
    for ClickFrame<T, P, K, S, A, C>
{
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.body, &other.body)
            && self.body_version == other.body_version
            && Arc::ptr_eq(&self.projection_epoch, &other.projection_epoch)
            && Arc::ptr_eq(&self.panel_epoch, &other.panel_epoch)
            && self.target == other.target
            && self.projection == other.projection
            && self.selection_generation == other.selection_generation
            && self.selection == other.selection
            && self.cursor == other.cursor
            && self.color == other.color
            && self.style == other.style
            && self.action == other.action
            && self.context == other.context
    }
}

struct Pending<P> {
    token: Arc<()>,
    widget: &'static str,
    down: DownProof,
    payload: P,
}

pub(crate) struct ClickSlot<P> {
    pending: Option<Pending<P>>,
}

impl<P> Default for ClickSlot<P> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<P> ClickSlot<P> {
    pub(crate) fn cancel(&mut self) {
        self.pending = None;
    }

    pub(crate) fn cancel_token(&mut self, token: &Arc<()>) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(&p.token, token))
        {
            self.cancel();
        }
    }

    pub(crate) fn arm(
        &mut self,
        token: Arc<()>,
        widget: &'static str,
        down: DownProof,
        payload: P,
    ) {
        self.cancel();
        if down.left && !down.first_mouse {
            self.pending = Some(Pending {
                token,
                widget,
                down,
                payload,
            });
        }
    }

    pub(crate) fn payload(&self) -> Option<&P> {
        self.pending.as_ref().map(|p| &p.payload)
    }

    /// 每次 Click 均消费本次槽，包括拒绝；旧事件不能留待未来状态恢复再执行。
    pub(crate) fn claim(
        &mut self,
        widget: &'static str,
        down: Option<DownProof>,
        left_pair: bool,
        current: impl FnOnce(&P) -> bool,
    ) -> Option<P> {
        let pending = self.pending.take()?;
        (left_pair
            && pending.widget == widget
            && down == Some(pending.down)
            && current(&pending.payload))
        .then_some(pending.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Frame = ClickFrame<u64, [i32; 4], (usize, usize), (u32, u32), u8, u8>;
    fn frame() -> Frame {
        Frame {
            body: Arc::new(()),
            body_version: (2, 3),
            projection_epoch: Arc::new(()),
            panel_epoch: Arc::new(()),
            target: 7,
            projection: [125, -30, 1200, 800],
            selection_generation: 9,
            selection: Some(0..12),
            cursor: Some((0, 12)),
            color: Some(0xabcdef),
            style: (0xff0000, 4),
            action: 1,
            context: 2,
        }
    }
    fn down() -> DownProof {
        DownProof {
            left: true,
            position: [10.5f32.to_bits(), (-30.25f32).to_bits()],
            modifiers: 4,
            click_count: 1,
            first_mouse: false,
        }
    }
    fn armed(saved: Frame) -> ClickSlot<Frame> {
        let mut slot = ClickSlot::default();
        slot.arm(Arc::new(()), "mark", down(), saved);
        slot
    }

    #[test]
    fn matching_down_and_intent_are_consumed_once() {
        let saved = frame();
        let mut slot = armed(saved.clone());
        assert!(slot
            .claim("mark", Some(down()), true, |p| p == &saved)
            .is_some());
        assert!(slot.claim("mark", Some(down()), true, |_| true).is_none());
    }

    #[test]
    fn gpui_retained_old_down_cannot_adopt_new_render_after_zoom_aba() {
        let saved = frame();
        let mut restored = saved.clone();
        restored.projection_epoch = Arc::new(());
        assert_eq!(saved.projection, restored.projection);
        let mut slot = armed(saved);
        assert!(slot
            .claim("mark", Some(down()), true, |p| p == &restored)
            .is_none());
        assert!(slot.payload().is_none());
    }

    #[test]
    fn equal_body_bytes_cannot_resurrect_replaced_source_or_body() {
        let saved = frame();
        for replaced_body in [false, true] {
            let mut current = saved.clone();
            if replaced_body {
                current.body = Arc::new(());
            } else {
                current.target += 1;
            }
            assert!(armed(saved.clone())
                .claim("mark", Some(down()), true, |p| p == &current)
                .is_none());
        }
    }

    #[test]
    fn changed_operation_selection_cursor_style_color_and_panel_intent_reject() {
        let saved = frame();
        let mut changed = Vec::new();
        let mut f = saved.clone();
        f.action += 1;
        changed.push(f);
        let mut f = saved.clone();
        f.selection = Some(1..12);
        changed.push(f);
        let mut f = saved.clone();
        f.cursor = Some((1, 12));
        changed.push(f);
        let mut f = saved.clone();
        f.style.1 += 1;
        changed.push(f);
        let mut f = saved.clone();
        f.color = None;
        changed.push(f);
        let mut f = saved.clone();
        f.context += 1;
        changed.push(f);
        let mut f = saved.clone();
        f.panel_epoch = Arc::new(());
        changed.push(f);
        let mut f = saved.clone();
        f.body_version.1 += 1;
        changed.push(f);
        let mut f = saved.clone();
        f.selection_generation += 1;
        changed.push(f);
        for current in changed {
            assert!(armed(saved.clone())
                .claim("mark", Some(down()), true, |p| p == &current)
                .is_none());
        }
    }

    #[test]
    fn different_widget_or_mouse_down_does_not_claim_saved_operation() {
        let saved = frame();
        assert!(armed(saved.clone())
            .claim("copy", Some(down()), true, |_| true)
            .is_none());
        for kind in 0..4 {
            let mut changed = down();
            match kind {
                0 => changed.position[0] += 1,
                1 => changed.modifiers ^= 1,
                2 => changed.click_count += 1,
                _ => changed.first_mouse = true,
            }
            assert!(armed(saved.clone())
                .claim("mark", Some(changed), true, |_| true)
                .is_none());
        }
    }

    #[test]
    fn keyboard_and_mixed_mouse_pairs_reject_without_retaining_slot() {
        for (proof, left_pair) in [(None, false), (Some(down()), false)] {
            let mut slot = armed(frame());
            assert!(slot.claim("mark", proof, left_pair, |_| true).is_none());
            assert!(slot.payload().is_none());
        }
    }

    #[test]
    fn inactive_ime_save_or_unpainted_access_consumes_intent() {
        for _blocked_access in ["inactive", "ime", "save", "unpainted", "busy"] {
            let mut slot = armed(frame());
            assert!(slot.claim("mark", Some(down()), true, |_| false).is_none());
            assert!(slot.payload().is_none());
        }
    }

    #[test]
    fn old_blur_subscription_cannot_cancel_new_down_but_current_blur_can() {
        let old = Arc::new(());
        let current = Arc::new(());
        let mut slot = ClickSlot::default();
        slot.arm(old.clone(), "copy", down(), frame());
        slot.arm(current.clone(), "mark", down(), frame());
        slot.cancel_token(&old);
        assert!(slot.payload().is_some());
        slot.cancel_token(&current);
        assert!(slot.payload().is_none());
    }

    #[test]
    fn explicit_mutation_cancel_survives_state_aba() {
        let saved = frame();
        let mut slot = armed(saved.clone());
        slot.cancel();
        assert!(slot
            .claim("mark", Some(down()), true, |p| p == &saved)
            .is_none());
    }

    #[test]
    fn right_down_and_first_focusing_down_are_never_armed() {
        for invalid in [
            DownProof {
                left: false,
                ..down()
            },
            DownProof {
                first_mouse: true,
                ..down()
            },
        ] {
            let mut slot = ClickSlot::default();
            slot.arm(Arc::new(()), "mark", invalid, frame());
            assert!(slot.payload().is_none());
        }
    }
}
