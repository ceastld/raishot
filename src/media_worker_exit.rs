//! 播放后台工作者的退出回执；观察者只保留标记，不延长媒体或界面资源寿命。
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone)]
pub(crate) struct WorkerExit(Arc<AtomicBool>);

impl WorkerExit {
    pub(crate) fn pending() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub(crate) fn completed() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
    pub(crate) fn is_exited(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub(crate) fn mark_exited(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// 工作线程先创建本 guard；媒体读取与设备资源须位于其后的内层作用域中。
    pub(crate) fn guard(&self) -> WorkerExitGuard {
        WorkerExitGuard(self.clone())
    }
}

pub(crate) struct WorkerExitGuard(WorkerExit);
impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        self.0.mark_exited();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_exit_probe_waits_for_scope_exit_even_during_unwind() {
        let exit = WorkerExit::pending();
        let probe = exit.clone();
        let result = std::panic::catch_unwind(|| {
            let _guard = exit.guard();
            assert!(!probe.is_exited());
            panic!("模拟后台错误");
        });
        assert!(result.is_err());
        assert!(probe.is_exited());
        assert!(WorkerExit::completed().is_exited());
    }
}
