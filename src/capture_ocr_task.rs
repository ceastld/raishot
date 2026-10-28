//! Capture OCR 宿主的单一任务槽；身份过滤与工作回执分类先于 Owner 释放。
use crate::recognition::task::{is_cancelled, CancelOwner, TaskToken};

/// 工作函数必须在把错误转为字符串之前构造回执。
/// 不使用令牌布尔值重写真实错误，也不在 Owner 释放后重新判断已完成回执。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Receipt<T> {
    Completed(T),
    Cancelled,
    Failed(String),
}

impl<T> Receipt<T> {
    pub(crate) fn from_result(result: crate::Result<T>) -> Self {
        match result {
            Ok(value) => Self::Completed(value),
            Err(error) if is_cancelled(error.as_ref()) => Self::Cancelled,
            Err(error) => Self::Failed(error.to_string()),
        }
    }

    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> Receipt<U> {
        match self {
            Self::Completed(value) => Receipt::Completed(map(value)),
            Self::Cancelled => Receipt::Cancelled,
            Self::Failed(error) => Receipt::Failed(error),
        }
    }
}

/// 可交给工作函数的副本；只携带令牌，不拥有取消权。
#[derive(Clone)]
pub(crate) struct TaskRequest<K> {
    pub(crate) id: u64,
    pub(crate) key: K,
    pub(crate) token: TaskToken,
}

struct Active<K> {
    id: u64,
    key: K,
    owner: CancelOwner,
}

pub(crate) struct TaskSlot<K> {
    serial: u64,
    active: Option<Active<K>>,
}

impl<K> Default for TaskSlot<K> {
    fn default() -> Self {
        Self {
            serial: 0,
            active: None,
        }
    }
}

impl<K> TaskSlot<K> {
    pub(crate) fn start(&mut self, key: K) -> TaskRequest<K>
    where
        K: Clone,
    {
        // 必须先取得不可复用的身份；耗尽时不先取消仍有效的旧任务。
        let id = self.serial.checked_add(1).expect("OCR 任务身份计数耗尽");
        self.cancel("请求替换");
        self.serial = id;
        let owner = CancelOwner::new();
        let request = TaskRequest {
            id,
            key: key.clone(),
            token: owner.token(),
        };
        self.active = Some(Active { id, key, owner });
        request
    }

    pub(crate) fn owns(&self, id: u64) -> bool {
        self.active.as_ref().is_some_and(|active| active.id == id)
    }

    pub(crate) fn key(&self) -> Option<&K> {
        self.active.as_ref().map(|active| &active.key)
    }

    pub(crate) fn current_id(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.id)
    }

    pub(crate) fn token(&self) -> Option<TaskToken> {
        self.active.as_ref().map(|active| active.owner.token())
    }

    /// 仅释放匹配身份；调用者必须已经核定回执和相关 source/ROI 合同。
    /// Owner 释放会标记令牌取消，不能再据此否定已经收到的 Completed。
    pub(crate) fn finish(&mut self, id: u64) -> bool {
        if !self.owns(id) {
            return false;
        }
        let active = self.active.take().expect("匹配的 OCR 任务槽必须非空");
        release(active, "匹配回执已返回，释放任务拥有者");
        true
    }

    /// 接收已经三态分类的回执，然后释放匹配 Owner；旧回执完全不触动新任务。
    pub(crate) fn finish_receipt<T>(&mut self, id: u64, receipt: Receipt<T>) -> Option<Receipt<T>> {
        self.finish(id).then_some(receipt)
    }

    /// 取消仅涉及本槽的当前任务。记录控制错误不代表原生推理已退出。
    pub(crate) fn cancel(&mut self, reason: &str) -> bool {
        let Some(active) = self.active.take() else {
            return false;
        };
        release(active, reason);
        true
    }
}

fn cancellation_failure_message(id: u64, reason: &str, error: &dyn std::error::Error) -> String {
    format!(
        "OCR 任务取消控制调用失败 [id={id}, reason={reason}]: {error}；已登记取消，未确认原生推理已退出"
    )
}

fn release<K>(active: Active<K>, reason: &str) {
    // CancelOwner 的 Drop 只能忽略错误。宿主主动释放时先记录本任务的控制错误；
    // 随后的 Owner Drop 是重复取消，不会再次调用活动终止目标。
    if let Err(error) = active.owner.cancel() {
        eprintln!(
            "{}",
            cancellation_failure_message(active.id, reason, error.as_ref())
        );
    }
}

impl<K> Drop for TaskSlot<K> {
    fn drop(&mut self) {
        self.cancel("任务槽析构兜底");
    }
}

#[cfg(test)]
#[path = "capture_ocr_task_tests.rs"]
mod tests;
