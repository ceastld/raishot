//! OCR 依赖下载进度：进程内原子量，界面在 paint 中轮询；不引入 GPUI 类型。
//! 只在持有准备锁时写入，同一时刻至多一个准备任务；读侧容忍一帧内的撕裂。
use std::{
    io::Read,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

static TRACKER: Tracker = Tracker::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DownloadProgress {
    pub(crate) received: u64,
    pub(crate) total: u64,
}

impl DownloadProgress {
    /// “已下载 / 本次待下载总量”，按 MiB 保留一位小数。
    pub(crate) fn amount(self) -> String {
        const MIB: f64 = 1024. * 1024.;
        format!(
            "{:.1} / {:.1} MB",
            self.received.min(self.total) as f64 / MIB,
            self.total as f64 / MIB
        )
    }

    /// 完整文案在前，空间不足时退到短文案。
    pub(crate) fn labels(self) -> [String; 2] {
        let amount = self.amount();
        [
            trf!(
                "首次使用，正在下载文字识别模型 {}",
                "Downloading OCR models (first use) {}",
                amount
            ),
            trf!("下载模型 {}", "Models {}", amount),
        ]
    }
}

/// 正在联网下载时返回进度；校验缓存、加载模型或识别阶段均为 None。
pub(crate) fn current() -> Option<DownloadProgress> {
    TRACKER.current()
}

pub(super) fn plan(planned: u64) -> Plan<'static> {
    TRACKER.plan(planned)
}

pub(super) fn begin(bytes: u64) {
    TRACKER.begin(bytes);
}

pub(super) fn counting<R: Read>(inner: R) -> Counting<'static, R> {
    Counting {
        inner,
        tracker: &TRACKER,
    }
}

pub(super) struct Tracker {
    planned: AtomicU64,
    total: AtomicU64,
    received: AtomicU64,
}

impl Tracker {
    pub(super) const fn new() -> Self {
        Self {
            planned: AtomicU64::new(0),
            total: AtomicU64::new(0),
            received: AtomicU64::new(0),
        }
    }

    fn current(&self) -> Option<DownloadProgress> {
        let total = self.total.load(Relaxed);
        (total > 0).then(|| DownloadProgress {
            received: self.received.load(Relaxed).min(total),
            total,
        })
    }

    /// 准备锁内登记缺失文件的固定总字节；先不公布，真正联网时才显示。
    fn plan(&self, planned: u64) -> Plan<'_> {
        self.reset();
        self.planned.store(planned, Relaxed);
        Plan { tracker: self }
    }

    /// 缓存校验未通过、即将请求网络时调用；总量不小于已收字节加本文件。
    fn begin(&self, bytes: u64) {
        let needed = self.received.load(Relaxed).saturating_add(bytes);
        let total = self
            .total
            .load(Relaxed)
            .max(self.planned.load(Relaxed))
            .max(needed);
        self.total.store(total, Relaxed);
    }

    fn reset(&self) {
        self.total.store(0, Relaxed);
        self.received.store(0, Relaxed);
        self.planned.store(0, Relaxed);
    }
}

/// 释放时清空进度；成功、失败与取消都回到普通“识别中”反馈。
pub(super) struct Plan<'a> {
    tracker: &'a Tracker,
}

impl Drop for Plan<'_> {
    fn drop(&mut self) {
        self.tracker.reset();
    }
}

/// 只计数，不改变读取语义；大小与 SHA256 仍由调用方校验。
pub(super) struct Counting<'a, R> {
    inner: R,
    tracker: &'a Tracker,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.tracker.received.fetch_add(count as u64, Relaxed);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_hidden_until_network_and_cleared_by_plan_drop() {
        let tracker = Tracker::new();
        let plan = tracker.plan(300);
        assert_eq!(tracker.current(), None);
        tracker.begin(100);
        let mut reader = Counting {
            inner: &[7u8; 150][..],
            tracker: &tracker,
        };
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();
        assert_eq!(
            tracker.current(),
            Some(DownloadProgress {
                received: 150,
                total: 300
            })
        );
        // 计划外的下载只把总量撑大，已下载量不会超过总量。
        tracker.begin(400);
        assert_eq!(tracker.current().unwrap().total, 550);
        drop(plan);
        assert_eq!(tracker.current(), None);
    }

    #[test]
    fn amount_clamps_and_labels_carry_value() {
        let progress = DownloadProgress {
            received: 9,
            total: 4,
        };
        assert_eq!(progress.amount(), "0.0 / 0.0 MB");
        let progress = DownloadProgress {
            received: 3_250_586,
            total: 7_654_604,
        };
        assert_eq!(progress.amount(), "3.1 / 7.3 MB");
        let labels = progress.labels();
        assert!(labels.iter().all(|label| label.ends_with("3.1 / 7.3 MB")));
        assert!(labels[1].chars().count() < labels[0].chars().count());
    }
}
