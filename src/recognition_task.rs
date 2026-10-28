//! 每请求取消权；Owner 控制自己的令牌，工作副本与注册析构不取消其它请求。
use crate::Result;
use parking_lot::{Mutex, MutexGuard};
use std::{
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Debug)]
pub struct OcrCancelled;

impl fmt::Display for OcrCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OCR 任务已取消")
    }
}

impl Error for OcrCancelled {}

pub fn is_cancelled(error: &(dyn Error + 'static)) -> bool {
    error.downcast_ref::<OcrCancelled>().is_some()
}

/// 后端注册的本任务终止目标；控制调用在注册锁之外，不等待 OCR 引擎锁。
pub(crate) trait RunCancellation: Send + Sync {
    fn terminate(&self) -> Result<()>;
}

struct TaskState {
    cancelled: AtomicBool,
    active: Mutex<Option<Arc<dyn RunCancellation>>>,
}

/// 单一宿主持有者；正常完成后释放工作注册，真正替换／关闭时析构取消此请求。
pub struct CancelOwner {
    token: TaskToken,
}

#[derive(Clone)]
pub struct TaskToken {
    state: Arc<TaskState>,
}

impl Default for CancelOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelOwner {
    pub fn new() -> Self {
        Self {
            token: TaskToken {
                state: Arc::new(TaskState {
                    cancelled: AtomicBool::new(false),
                    active: Mutex::new(None),
                }),
            },
        }
    }

    pub fn token(&self) -> TaskToken {
        self.token.clone()
    }

    /// 首次请求返回 true；重复返回 false。原生终止控制错误原样返回。
    /// 接受请求不表示 Run 已退出，且不表示 HTTP 或 session 构造能即时停止。
    pub fn cancel(&self) -> Result<bool> {
        let active = {
            let registered = self.token.state.active.lock();
            if self.token.state.cancelled.swap(true, Ordering::AcqRel) {
                return Ok(false);
            }
            registered.clone()
        };
        if let Some(active) = active {
            active.terminate()?;
        }
        Ok(true)
    }
}

impl Drop for CancelOwner {
    fn drop(&mut self) {
        // 析构只做尽力终止；它不能报告控制错误，亦不资格化原生退出成功。
        let _ = self.cancel();
    }
}

impl TaskToken {
    pub fn checkpoint(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(OcrCancelled.into())
        } else {
            Ok(())
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn register(&self, control: Arc<dyn RunCancellation>) -> Result<RunRegistration> {
        let mut active = self.state.active.lock();
        self.checkpoint()?;
        if active.is_some() {
            return Err("当前 OCR 请求已有活动推理选项".into());
        }
        *active = Some(control.clone());
        Ok(RunRegistration {
            token: self.clone(),
            control,
        })
    }
}

pub(crate) struct RunRegistration {
    token: TaskToken,
    control: Arc<dyn RunCancellation>,
}

impl Drop for RunRegistration {
    fn drop(&mut self) {
        let mut active = self.token.state.active.lock();
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.control))
        {
            *active = None;
        }
    }
}

/// 取消排队请求不得进入后续下载／加载；获取锁后再次检查注册竞态。
pub(crate) fn wait_for_lock<'a, T>(
    mutex: &'a Mutex<T>,
    task: &TaskToken,
) -> Result<MutexGuard<'a, T>> {
    loop {
        task.checkpoint()?;
        if let Some(guard) = mutex.try_lock_for(Duration::from_millis(10)) {
            task.checkpoint()?;
            return Ok(guard);
        }
    }
}

#[cfg(test)]
#[path = "recognition_task_tests.rs"]
mod tests;
