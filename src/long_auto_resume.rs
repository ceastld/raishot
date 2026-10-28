//! 设置浮层只暂停自动滚动；关闭后恢复原绑定的意图，不重新发现页面目标。
use crate::{geometry::ScreenRect, long_auto::AutoScrollOptions, stitch::ScrollAxis, Result};

/// T 必须标识原 committed 捕获/目标绑定，不能用窗口标题或显示标签代替。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResumeContext<T> {
    pub(crate) region: ScreenRect,
    pub(crate) axis: ScrollAxis,
    pub(crate) generation: u64,
    pub(crate) target: T,
}

struct Intent<T> {
    context: ResumeContext<T>,
    pause_generation: u64,
    options: AutoScrollOptions,
    closed: bool,
}

pub(crate) struct AutoResume<T> {
    intent: Option<Intent<T>>,
}
impl<T> Default for AutoResume<T> {
    fn default() -> Self {
        Self { intent: None }
    }
}
impl<T: PartialEq> AutoResume<T> {
    /// pause_generation 来自成功发布的单次 Pause；记录 active 或尚未启用的 pending。
    pub(crate) fn suspend(
        &mut self,
        active: bool,
        pending: bool,
        options: AutoScrollOptions,
        context: ResumeContext<T>,
        pause_generation: u64,
    ) -> Result<bool> {
        if self.intent.is_some() {
            return Err("设置的恢复请求仍在等待；请复用当前暂停".into());
        }
        if !active && !pending {
            return Ok(false);
        }
        if context.generation.checked_add(1) != Some(pause_generation) {
            return Err("设置的暂停版本已失效".into());
        }
        self.intent = Some(Intent {
            context,
            pause_generation,
            options,
            closed: false,
        });
        Ok(true)
    }
    pub(crate) fn close(&mut self) {
        if let Some(intent) = self.intent.as_mut() {
            intent.closed = true;
        }
    }
    /// 关闭后很快重开时保留原 Pause 的代次，不再发布第二次暂停。
    pub(crate) fn reopen(&mut self) {
        if let Some(intent) = self.intent.as_mut() {
            intent.closed = false;
        }
    }
    /// 仅传入当前 committed 上下文。目标现场校验失败须由调用方 invalidate；
    /// 过渡代尚不能读取上下文时只等待；就绪检查不消费原绑定，启动成功后才 invalidate。
    pub(crate) fn ready_options(
        &mut self,
        current: &ResumeContext<T>,
        quiesced: bool,
    ) -> Option<AutoScrollOptions> {
        let intent = self.intent.as_ref()?;
        if current.generation > intent.pause_generation
            || current.region != intent.context.region
            || current.axis != intent.context.axis
            || current.target != intent.context.target
        {
            self.invalidate();
            return None;
        }
        if !intent.closed || current.generation != intent.pause_generation || !quiesced {
            return None;
        }
        Some(intent.options)
    }
    /// 纯模型验证一次性交付；生产异步启用须保留意图直到原生启动成功。
    pub(crate) fn take_ready(
        &mut self,
        current: &ResumeContext<T>,
        quiesced: bool,
    ) -> Option<AutoScrollOptions> {
        self.ready_options(current, quiesced)?;
        self.intent.take().map(|intent| intent.options)
    }
    /// 显式 Stop/Pause、编辑/输出、关闭、重来、撤销或选区/轴/方向变更均作废意图。
    pub(crate) fn invalidate(&mut self) {
        self.intent = None;
    }
    pub(crate) fn is_pending(&self) -> bool {
        self.intent.is_some()
    }
    pub(crate) fn expected_generation(&self) -> Option<u64> {
        self.intent.as_ref().map(|intent| intent.pause_generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::long_auto::AutoDirection;

    fn context(generation: u64) -> ResumeContext<u64> {
        ResumeContext {
            region: ScreenRect {
                left: -960,
                top: 80,
                width: 800,
                height: 640,
            },
            axis: ScrollAxis::Vertical,
            generation,
            target: 71,
        }
    }

    #[test]
    fn long_auto_resume_active_or_pending_restores_direction_once_on_original_binding() {
        for (active, pending) in [(true, false), (false, true), (true, true), (false, false)] {
            let mut resume = AutoResume::default();
            let options = AutoScrollOptions {
                direction: AutoDirection::Reverse,
            };
            assert_eq!(
                resume
                    .suspend(active, pending, options, context(41), 42)
                    .unwrap(),
                active || pending
            );
            if !active && !pending {
                resume.close();
                assert_eq!(resume.take_ready(&context(42), true), None);
                assert!(!resume.is_pending());
                continue;
            }
            assert_eq!(resume.expected_generation(), Some(42));
            assert_eq!(
                resume.take_ready(&context(42), true),
                None,
                "浮层打开时不能恢复"
            );
            resume.close();
            assert_eq!(resume.ready_options(&context(42), true), Some(options));
            assert_eq!(resume.ready_options(&context(42), true), Some(options));
            assert!(resume.is_pending(), "异步焦点/原生启动前必须保留原绑定");
            resume.reopen();
            assert_eq!(resume.ready_options(&context(42), true), None);
            assert!(resume.is_pending());
            resume.close();
            assert_eq!(resume.take_ready(&context(42), true), Some(options));
            assert!(!resume.is_pending());
            assert_eq!(resume.expected_generation(), None);
            assert_eq!(
                resume.take_ready(&context(42), true),
                None,
                "一次关闭只可恢复一次"
            );
        }
    }

    #[test]
    fn long_auto_resume_close_before_ack_and_reopen_keep_the_original_pause_epoch() {
        let mut resume = AutoResume::default();
        resume
            .suspend(true, false, AutoScrollOptions::default(), context(41), 42)
            .unwrap();
        resume.close();
        assert_eq!(
            resume.take_ready(&context(41), true),
            None,
            "旧代的安静状态不是新 Pause ACK"
        );
        assert!(resume.is_pending());
        assert_eq!(
            resume.take_ready(&context(42), false),
            None,
            "ACK 后在途捕获仍需等待"
        );
        resume.reopen();
        assert_eq!(resume.expected_generation(), Some(42));
        assert_eq!(resume.ready_options(&context(42), true), None);
        assert_eq!(resume.take_ready(&context(42), true), None);
        // 原 ACK 到达后再次关闭，沿用同一意图，不发布额外的 Pause 版本。
        resume.close();
        assert_eq!(
            resume.take_ready(&context(42), true),
            Some(AutoScrollOptions::default())
        );
    }

    #[test]
    fn long_auto_resume_changed_roi_axis_target_or_newer_request_invalidates_instead_of_rebinding()
    {
        let mut moved = context(42);
        moved.region.left += 1;
        let mut resized = context(42);
        resized.region.width -= 1;
        let mut axis = context(42);
        axis.axis = ScrollAxis::Horizontal;
        let mut target = context(42);
        target.target = 72;
        for changed in [moved, resized, axis, target, context(43)] {
            let mut resume = AutoResume::default();
            resume
                .suspend(true, false, AutoScrollOptions::default(), context(41), 42)
                .unwrap();
            resume.close();
            assert_eq!(resume.ready_options(&changed, true), None);
            assert!(!resume.is_pending(), "安全边界变化后不得继续保留恢复请求");
            assert_eq!(
                resume.take_ready(&context(42), true),
                None,
                "旧身份重新出现也不能复活已作废意图"
            );
        }
    }

    #[test]
    fn long_auto_resume_explicit_cancel_and_invalid_inputs_cannot_revive_or_overwrite_intent() {
        let mut resume = AutoResume::default();
        let options = AutoScrollOptions {
            direction: AutoDirection::Reverse,
        };
        resume
            .suspend(false, true, options, context(41), 42)
            .unwrap();
        assert!(resume
            .suspend(true, false, AutoScrollOptions::default(), context(43), 44)
            .is_err());
        assert_eq!(
            resume.expected_generation(),
            Some(42),
            "失败输入不覆盖旧请求"
        );
        resume.close();
        assert_eq!(resume.ready_options(&context(42), true), Some(options));
        assert_eq!(resume.ready_options(&context(42), true), Some(options));
        assert!(resume.is_pending());
        resume.invalidate();
        assert_eq!(
            resume.take_ready(&context(42), true),
            None,
            "显式取消后迟到 ACK 不能重新启动"
        );
        assert!(!resume.is_pending());
        assert!(resume
            .suspend(true, false, options, context(41), 43)
            .is_err());
        assert!(resume
            .suspend(true, false, options, context(u64::MAX), 0)
            .is_err());
        assert!(!resume.is_pending());
        // 新的独立打开仍可建立意图，旧 ACK 不可提前消费它。
        resume
            .suspend(true, false, options, context(43), 44)
            .unwrap();
        resume.close();
        assert_eq!(resume.take_ready(&context(42), true), None);
        assert_eq!(resume.take_ready(&context(44), true), Some(options));
    }
}
