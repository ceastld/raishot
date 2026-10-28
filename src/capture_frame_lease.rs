//! 已成功 Acquire 的帧租约；提交拷贝后只凭完成事件或真实设备失效归还。
//!
//! 两秒是失败报告门，不是 GPU 完成门。超时、查询错误及取消均继续排空；
//! 若驱动永远不完成且始终声称设备有效，本模块不能保证有限停止墙钟时间。
//! Backend 保有 duplication、device、context、query 及拷贝源/目标的所有权，
//! 不允许在排空期间复用资源，也不新增线程或隔离、遗忘租约。
use crate::{capture::cancellation::CaptureCancelled, Result};
use std::{cell::Cell, error::Error, fmt, io::Write, time::Duration};

const FAILURE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum QueryStatus {
    Pending,
    Complete,
}

/// 调用均由采集所有者执行。seal/poll/removed/pause/release 不得 panic。
/// poll 的 Complete 必须同时满足底层成功完成状态与事件 BOOL 为真；
/// removed 的 Ok 仅表示设备仍有效，绝不表示拷贝完成。
pub(super) trait Backend {
    /// 为此前提交的所有拷贝 End EVENT 并 Flush；只调用一次。
    fn seal(&mut self);
    fn poll(&mut self) -> Result<QueryStatus>;
    /// 只有真实失败返回值才确认旧设备已失效。
    fn removed(&mut self) -> Result<()>;
    /// 从 seal 起的单调经过时间；正常完成后不再读取。
    fn elapsed(&self) -> Duration;
    fn pause(&mut self, failing: bool);
    fn release(&mut self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Acquired,
    Submitted,
    Sealed,
    ReleaseAttempted,
}

pub(super) struct FrameLease<B: Backend> {
    backend: B,
    phase: Phase,
}

impl<B: Backend> FrameLease<B> {
    /// 只能在 Acquire 成功之后立即创建；之后所有出口均经过租约。
    pub(super) fn new(backend: B) -> Self {
        Self {
            backend,
            phase: Phase::Acquired,
        }
    }

    /// 只供拷贝前保存源/目标的强引用或准备 EVENT，不能绕过 submit_copy 提交。
    pub(super) fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub(super) fn submit_copy(&mut self, copy: impl FnOnce()) -> Result<()> {
        if self.phase != Phase::Acquired {
            return Err("桌面帧租约不能重复提交拷贝".into());
        }
        // Copy* 是异步 void 调用；闭包即使 unwind，也必须按可能已提交排空。
        self.phase = Phase::Submitted;
        copy();
        self.seal();
        Ok(())
    }

    pub(super) fn finish<T>(
        mut self,
        result: Result<T>,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<T> {
        let mut failures = Failures::default();
        let value = match result {
            Ok(value) => Some(value),
            Err(error) => {
                let stage = if error.is::<CaptureCancelled>() {
                    Stage::Cancellation
                } else {
                    Stage::Body
                };
                failures.remember(stage, error);
                None
            }
        };
        failures.observe_cancellation(cancelled);
        self.drain(&mut failures, cancelled);
        // 最后一次观察位于 Release 尝试之后，不能把取消作为提前归还的证据。
        failures.observe_cancellation(cancelled);
        match failures.into_error() {
            Some(error) => Err(error),
            None => Ok(value.expect("无错误的帧租约必须保有成功结果")),
        }
    }

    fn seal(&mut self) {
        if self.phase == Phase::Submitted {
            // 标记在调用前，防止不符合 Backend 合同的 panic 导致重复 End。
            self.phase = Phase::Sealed;
            self.backend.seal();
        }
    }

    fn drain(&mut self, failures: &mut Failures, cancelled: Option<&dyn Fn() -> bool>) {
        if self.phase == Phase::ReleaseAttempted {
            return;
        }
        self.seal();
        if self.phase == Phase::Sealed {
            loop {
                match self.backend.poll() {
                    Ok(QueryStatus::Complete) => break,
                    Ok(QueryStatus::Pending) => {}
                    Err(error) => failures.remember(Stage::Query, error),
                }
                // 查询错误或本地超时均不是设备失效证据；设备仍有效时继续等待。
                match self.backend.removed() {
                    Ok(()) => {}
                    Err(error) => {
                        failures.remember(Stage::Device, error);
                        break;
                    }
                }
                if self.backend.elapsed() >= FAILURE_TIMEOUT {
                    failures.remember_timeout();
                }
                failures.observe_cancellation(cancelled);
                self.backend.pause(failures.failing());
            }
        }
        // Acquired 尚未提交，或上方已取得真实完成/旧设备失效证据。
        // 即使没有拷贝或 EVENT 已完成，也不能让同遇的真实设备失败被取消吞掉。
        // 此检查只补诊断；pending 路径不能通过 S_OK 设备状态走到这里。
        if !failures.has(Stage::Device) {
            if let Err(error) = self.backend.removed() {
                failures.remember(Stage::Device, error);
            }
        }
        // 无论 Release 成败都仅尝试一次；Drop 不能再次调用 ReleaseFrame。
        self.phase = Phase::ReleaseAttempted;
        if let Err(error) = self.backend.release() {
            failures.remember(Stage::Release, error);
        }
    }
}

impl<B: Backend> Drop for FrameLease<B> {
    fn drop(&mut self) {
        if self.phase == Phase::ReleaseAttempted {
            return;
        }
        // 显式 finish 为通常路线；这里只处理早退/拷贝闭包 unwind。
        let mut failures = Failures::default();
        self.drain(&mut failures, None);
        if let Some(error) = failures.into_error() {
            let _ = writeln!(std::io::stderr().lock(), "桌面帧租约异常收尾：{error}");
        }
    }
}

/// 单采集通道的永久故障门；暂停/旧代次的健康取消仍可复用通道。
/// 参数检查必须位于门外，仅观察 Acquire 到租约 finish 的真实结果。
#[derive(Default)]
pub(super) struct LaneGate {
    failed: Cell<bool>,
}

impl LaneGate {
    pub(super) fn check(&self) -> Result<()> {
        if self.failed.get() {
            Err("桌面采集通道已失败，不能再次获取或提交帧".into())
        } else {
            Ok(())
        }
    }

    pub(super) fn observe<T>(&self, result: Result<T>) -> Result<T> {
        if result
            .as_ref()
            .is_err_and(|error| !error.is::<CaptureCancelled>())
        {
            self.failed.set(true);
        }
        result
    }
}

// 固定六个阶段；反复查询错误只保留首份，不随排空时长增长。
// 排序为真实正文/查询/设备/归还，其后才是本地超时与显式取消。
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
enum Stage {
    Body,
    Query,
    Device,
    Release,
    Timeout,
    Cancellation,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Body => "采集正文",
            Self::Query => "GPU 完成查询",
            Self::Device => "GPU 设备失效",
            Self::Release => "归还桌面帧",
            Self::Timeout => "GPU 本地等待超时",
            Self::Cancellation => "采集取消",
        }
    }

    fn from_index(index: usize) -> Self {
        [
            Self::Body,
            Self::Query,
            Self::Device,
            Self::Release,
            Self::Timeout,
            Self::Cancellation,
        ][index]
    }
}

#[derive(Default)]
struct Failures {
    slots: [Option<Box<dyn Error>>; 6],
}

impl Failures {
    fn has(&self, stage: Stage) -> bool {
        self.slots[stage as usize].is_some()
    }

    fn remember(&mut self, stage: Stage, error: Box<dyn Error>) {
        if self.slots[stage as usize].is_none() {
            self.slots[stage as usize] = Some(error);
        }
    }

    fn remember_timeout(&mut self) {
        if self.slots[Stage::Timeout as usize].is_none() {
            self.remember(Stage::Timeout, GpuWaitTimeout.into());
        }
    }

    fn observe_cancellation(&mut self, cancelled: Option<&dyn Fn() -> bool>) {
        if self.slots[Stage::Cancellation as usize].is_none()
            && cancelled.is_some_and(|cancelled| cancelled())
        {
            self.remember(Stage::Cancellation, CaptureCancelled.into());
        }
    }

    fn failing(&self) -> bool {
        self.slots[..=Stage::Timeout as usize]
            .iter()
            .any(Option::is_some)
    }

    fn into_error(mut self) -> Option<Box<dyn Error>> {
        let primary_index = self.slots.iter().position(Option::is_some)?;
        let primary = self.slots[primary_index].take().expect("首错误存在");
        if self.slots.iter().all(Option::is_none) {
            Some(primary)
        } else {
            Some(Box::new(LeaseFailure {
                primary,
                secondary: self.slots,
            }))
        }
    }
}

#[derive(Debug)]
struct GpuWaitTimeout;

impl fmt::Display for GpuWaitTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GPU 截图同步等待超过两秒")
    }
}

impl Error for GpuWaitTimeout {}

#[derive(Debug)]
struct LeaseFailure {
    primary: Box<dyn Error>,
    secondary: [Option<Box<dyn Error>>; 6],
}

impl fmt::Display for LeaseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.primary)?;
        for (index, error) in self.secondary.iter().enumerate() {
            if let Some(error) = error {
                write!(f, "；{}：{error}", Stage::from_index(index).label())?;
            }
        }
        Ok(())
    }
}

impl Error for LeaseFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        collections::VecDeque,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Trace {
        Acquire,
        Copy,
        Seal,
        Pending,
        Complete,
        QueryError(u32),
        DeviceAlive,
        DeviceError(u32),
        Pause(bool),
        Release,
        Encoder,
    }

    #[derive(Debug)]
    struct Failure(u32);

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "注入失败 {}", self.0)
        }
    }

    impl Error for Failure {}

    enum PollStep {
        Pending,
        Complete,
        Error(Box<dyn Error>),
    }

    struct FakeBackend {
        trace: Rc<RefCell<Vec<Trace>>>,
        polls: VecDeque<(u64, PollStep)>,
        devices: VecDeque<Option<Box<dyn Error>>>,
        release_error: Option<Box<dyn Error>>,
        elapsed: Duration,
    }

    impl Backend for FakeBackend {
        fn seal(&mut self) {
            self.trace.borrow_mut().push(Trace::Seal);
            self.elapsed = Duration::ZERO;
        }

        fn poll(&mut self) -> Result<QueryStatus> {
            let (millis, step) = self.polls.pop_front().expect("排空不能超过脚本完成门");
            self.elapsed = Duration::from_millis(millis);
            match step {
                PollStep::Pending => {
                    self.trace.borrow_mut().push(Trace::Pending);
                    Ok(QueryStatus::Pending)
                }
                PollStep::Complete => {
                    self.trace.borrow_mut().push(Trace::Complete);
                    Ok(QueryStatus::Complete)
                }
                PollStep::Error(error) => {
                    self.trace.borrow_mut().push(Trace::QueryError(
                        error.downcast_ref::<Failure>().unwrap().0,
                    ));
                    Err(error)
                }
            }
        }

        fn removed(&mut self) -> Result<()> {
            match self.devices.pop_front().unwrap_or(None) {
                Some(error) => {
                    self.trace.borrow_mut().push(Trace::DeviceError(
                        error.downcast_ref::<Failure>().unwrap().0,
                    ));
                    Err(error)
                }
                None => {
                    self.trace.borrow_mut().push(Trace::DeviceAlive);
                    Ok(())
                }
            }
        }

        fn elapsed(&self) -> Duration {
            self.elapsed
        }

        fn pause(&mut self, failing: bool) {
            self.trace.borrow_mut().push(Trace::Pause(failing));
        }

        fn release(&mut self) -> Result<()> {
            self.trace.borrow_mut().push(Trace::Release);
            match self.release_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    fn backend(polls: impl IntoIterator<Item = (u64, PollStep)>) -> FakeBackend {
        FakeBackend {
            trace: Rc::new(RefCell::new(vec![Trace::Acquire])),
            polls: polls.into_iter().collect(),
            devices: VecDeque::new(),
            release_error: None,
            elapsed: Duration::ZERO,
        }
    }

    fn submit(lease: &mut FrameLease<FakeBackend>, trace: &Rc<RefCell<Vec<Trace>>>) {
        lease
            .submit_copy(|| trace.borrow_mut().push(Trace::Copy))
            .unwrap();
    }

    /// 独立合同判断：不读取实现 Phase，只用外部操作的偏序与真实完成证据。
    fn assert_safe_once(trace: &[Trace]) {
        let releases = trace
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (*entry == Trace::Release).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(releases.len(), 1, "每个成功 Acquire 仅一次归还尝试");
        let release = releases[0];
        assert_eq!(trace[0], Trace::Acquire);
        if let Some(copy) = trace.iter().position(|entry| *entry == Trace::Copy) {
            let seals = trace
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| (*entry == Trace::Seal).then_some(index))
                .collect::<Vec<_>>();
            assert_eq!(seals.len(), 1, "已提交租约只允许一次 End/Flush");
            assert!(copy < seals[0] && seals[0] < release);
            assert!(
                trace[..release]
                    .iter()
                    .any(|entry| matches!(entry, Trace::Complete | Trace::DeviceError(_))),
                "已提交拷贝必须先完成或确认旧设备失效，超时/取消/查询错不是证明"
            );
        } else {
            assert!(!trace
                .iter()
                .any(|entry| matches!(entry, Trace::Seal | Trace::Pending | Trace::Complete)));
        }
        assert!(!trace[release + 1..].iter().any(|entry| matches!(
            entry,
            Trace::Copy | Trace::Seal | Trace::Pending | Trace::Complete | Trace::Release
        )));
    }

    fn primary<'a>(error: &'a (dyn Error + 'static)) -> &'a (dyn Error + 'static) {
        error
            .downcast_ref::<LeaseFailure>()
            .map_or(error, |error| error.primary.as_ref())
    }

    fn secondary_count(error: &(dyn Error + 'static)) -> usize {
        error
            .downcast_ref::<LeaseFailure>()
            .map_or(0, |error| error.secondary.iter().flatten().count())
    }

    #[test]
    fn unsubmitted_success_releases_without_gpu_poll() {
        let backend = backend([]);
        let trace = backend.trace.clone();
        let value = FrameLease::new(backend).finish(Ok(73), None).unwrap();
        assert_eq!(value, 73);
        assert_eq!(
            *trace.borrow(),
            [Trace::Acquire, Trace::DeviceAlive, Trace::Release]
        );
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn unsubmitted_body_error_keeps_original_box() {
        let backend = backend([]);
        let trace = backend.trace.clone();
        let failure: Box<dyn Error> = Box::new(Failure(11));
        let address = std::ptr::from_ref(failure.as_ref());
        let error = FrameLease::new(backend)
            .finish::<()>(Err(failure), None)
            .unwrap_err();
        assert!(std::ptr::eq(address, std::ptr::from_ref(error.as_ref())));
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn explicit_cancellation_without_copy_releases_and_stays_typed() {
        let backend = backend([]);
        let trace = backend.trace.clone();
        let error = FrameLease::new(backend)
            .finish::<()>(Err(CaptureCancelled.into()), None)
            .unwrap_err();
        assert!(error.is::<CaptureCancelled>());
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn normal_copy_completes_before_release_and_preserves_value() {
        let backend = backend([(1, PollStep::Pending), (2, PollStep::Complete)]);
        let trace = backend.trace.clone();
        let pixels = vec![1_u8, 2, 3];
        let original = pixels.as_ptr();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let returned = lease.finish(Ok(pixels), None).unwrap();
        assert_eq!(returned.as_ptr(), original);
        assert_safe_once(&trace.borrow());
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| **entry == Trace::DeviceAlive)
                .count(),
            2
        );
    }

    #[test]
    fn cancellation_after_copy_waits_for_completion() {
        let backend = backend([
            (1, PollStep::Pending),
            (2, PollStep::Pending),
            (3, PollStep::Complete),
        ]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), Some(&|| true)).unwrap_err();
        assert!(error.is::<CaptureCancelled>());
        assert_safe_once(&trace.borrow());
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| **entry == Trace::Pending)
                .count(),
            2
        );
    }

    #[test]
    fn timeout_then_completion_reports_failure_without_early_release() {
        let backend = backend([
            (2_000, PollStep::Pending),
            (3_000, PollStep::Pending),
            (3_001, PollStep::Complete),
        ]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), None).unwrap_err();
        assert!(error.is::<GpuWaitTimeout>());
        assert_safe_once(&trace.borrow());
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| **entry == Trace::Pause(true))
                .count(),
            2
        );
    }

    #[test]
    fn completion_at_timeout_boundary_is_completion() {
        let backend = backend([(2_000, PollStep::Complete)]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        lease.finish(Ok(()), None).unwrap();
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn query_error_with_live_device_still_waits_for_completion() {
        let failure: Box<dyn Error> = Box::new(Failure(31));
        let address = std::ptr::from_ref(failure.as_ref());
        let backend = backend([
            (1, PollStep::Error(failure)),
            (2, PollStep::Pending),
            (3, PollStep::Complete),
        ]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), None).unwrap_err();
        assert!(std::ptr::eq(address, std::ptr::from_ref(error.as_ref())));
        assert_safe_once(&trace.borrow());
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| **entry == Trace::DeviceAlive)
                .count(),
            3
        );
    }

    #[test]
    fn query_error_and_device_failure_keep_first_query_as_source() {
        let failure: Box<dyn Error> = Box::new(Failure(41));
        let address = std::ptr::from_ref(failure.as_ref());
        let mut backend = backend([(1, PollStep::Error(failure))]);
        backend.devices.push_back(Some(Failure(42).into()));
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), None).unwrap_err();
        assert!(std::ptr::eq(
            address,
            std::ptr::from_ref(error.source().unwrap())
        ));
        assert_eq!(secondary_count(error.as_ref()), 1);
        assert_safe_once(&trace.borrow());
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| matches!(entry, Trace::DeviceAlive | Trace::DeviceError(_)))
                .count(),
            1
        );
    }

    #[test]
    fn confirmed_device_failure_ends_drain_without_claiming_query_completion() {
        let device: Box<dyn Error> = Box::new(Failure(43));
        let address = std::ptr::from_ref(device.as_ref());
        let mut backend = backend([(1, PollStep::Pending)]);
        backend.devices.push_back(Some(device));
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), None).unwrap_err();
        assert!(std::ptr::eq(address, std::ptr::from_ref(error.as_ref())));
        assert_safe_once(&trace.borrow());
        assert!(!trace.borrow().contains(&Trace::Complete));
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| matches!(entry, Trace::DeviceAlive | Trace::DeviceError(_)))
                .count(),
            1
        );
    }

    #[test]
    fn live_device_status_alone_never_allows_release() {
        let backend = backend([
            (1, PollStep::Pending),
            (2, PollStep::Pending),
            (3, PollStep::Complete),
        ]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        lease.finish(Ok(()), None).unwrap();
        assert_safe_once(&trace.borrow());
        let trace = trace.borrow();
        let completed = trace
            .iter()
            .position(|entry| *entry == Trace::Complete)
            .unwrap();
        let release = trace
            .iter()
            .position(|entry| *entry == Trace::Release)
            .unwrap();
        assert!(completed < release);
    }

    #[test]
    fn timeout_then_real_device_failure_prefers_original_device_error() {
        let device_failure: Box<dyn Error> = Box::new(Failure(51));
        let address = std::ptr::from_ref(device_failure.as_ref());
        let mut backend = backend([(2_000, PollStep::Pending), (2_001, PollStep::Pending)]);
        backend.devices.extend([None, Some(device_failure)]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), Some(&|| true)).unwrap_err();
        assert!(std::ptr::eq(
            address,
            std::ptr::from_ref(error.source().unwrap())
        ));
        assert_eq!(secondary_count(error.as_ref()), 2);
        assert!(!error.is::<CaptureCancelled>());
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn release_error_is_not_swallowed_by_cancellation() {
        let mut backend = backend([(1, PollStep::Complete)]);
        backend.release_error = Some(Failure(61).into());
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), Some(&|| true)).unwrap_err();
        assert_eq!(
            primary(error.as_ref()).downcast_ref::<Failure>().unwrap().0,
            61
        );
        assert_eq!(secondary_count(error.as_ref()), 1);
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn no_copy_cancellation_with_device_failure_prefers_real_device_error() {
        let mut backend = backend([]);
        backend.devices.push_back(Some(Failure(65).into()));
        let trace = backend.trace.clone();
        let error = FrameLease::new(backend)
            .finish::<()>(Err(CaptureCancelled.into()), None)
            .unwrap_err();
        assert_eq!(
            primary(error.as_ref()).downcast_ref::<Failure>().unwrap().0,
            65
        );
        assert_eq!(secondary_count(error.as_ref()), 1);
        assert!(!error.is::<CaptureCancelled>());
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn completed_event_with_cancellation_does_not_hide_device_failure() {
        let mut backend = backend([(1, PollStep::Complete)]);
        backend.devices.push_back(Some(Failure(66).into()));
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), Some(&|| true)).unwrap_err();
        assert_eq!(
            primary(error.as_ref()).downcast_ref::<Failure>().unwrap().0,
            66
        );
        assert_eq!(secondary_count(error.as_ref()), 1);
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn body_error_stays_primary_before_all_cleanup_errors() {
        let body: Box<dyn Error> = Box::new(Failure(71));
        let address = std::ptr::from_ref(body.as_ref());
        let mut backend = backend([
            (2_000, PollStep::Pending),
            (2_001, PollStep::Error(Failure(72).into())),
        ]);
        backend.devices.extend([None, Some(Failure(73).into())]);
        backend.release_error = Some(Failure(74).into());
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish::<()>(Err(body), Some(&|| true)).unwrap_err();
        assert!(std::ptr::eq(
            address,
            std::ptr::from_ref(error.source().unwrap())
        ));
        assert_eq!(secondary_count(error.as_ref()), 5);
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn repeated_query_errors_have_fixed_storage_and_keep_first_object() {
        let query: Box<dyn Error> = Box::new(Failure(81));
        let address = std::ptr::from_ref(query.as_ref());
        let mut polls = vec![(1, PollStep::Error(query))];
        polls.extend((82..=181).map(|id| (2_000, PollStep::Error(Failure(id).into()))));
        polls.push((2_001, PollStep::Complete));
        let backend = backend(polls);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease.finish(Ok(()), Some(&|| true)).unwrap_err();
        assert!(std::ptr::eq(
            address,
            std::ptr::from_ref(error.source().unwrap())
        ));
        assert_eq!(secondary_count(error.as_ref()), 2);
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn no_copy_drop_attempts_release_exactly_once() {
        let backend = backend([]);
        let trace = backend.trace.clone();
        drop(FrameLease::new(backend));
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn submitted_drop_uses_the_same_completion_contract() {
        let backend = backend([(2_000, PollStep::Pending), (2_001, PollStep::Complete)]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        drop(lease);
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn explicit_finish_error_does_not_release_again_in_drop() {
        let mut backend = backend([(1, PollStep::Complete)]);
        backend.release_error = Some(Failure(191).into());
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        assert!(lease.finish(Ok(()), None).is_err());
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn copy_closure_unwind_seals_and_drains_before_release() {
        let backend = backend([(1, PollStep::Pending), (2, PollStep::Complete)]);
        let trace = backend.trace.clone();
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let mut lease = FrameLease::new(backend);
            let _ = lease.submit_copy(|| {
                trace.borrow_mut().push(Trace::Copy);
                panic!("注入拷贝闭包异常");
            });
        }));
        assert!(unwound.is_err());
        assert_safe_once(&trace.borrow());
    }

    #[test]
    fn second_copy_is_rejected_without_touching_backend() {
        let backend = backend([(1, PollStep::Complete)]);
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let error = lease
            .submit_copy(|| trace.borrow_mut().push(Trace::Copy))
            .unwrap_err();
        let error = lease.finish::<()>(Err(error), None).unwrap_err();
        assert!(error.to_string().contains("不能重复提交"));
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|entry| **entry == Trace::Copy)
                .count(),
            1
        );
        assert_safe_once(&trace.borrow());
    }

    fn guarded_frame(
        gate: &LaneGate,
        acquire: impl FnOnce() -> FakeBackend,
        cancel: bool,
    ) -> Result<()> {
        gate.check()?;
        let backend = acquire();
        let trace = backend.trace.clone();
        let mut lease = FrameLease::new(backend);
        submit(&mut lease, &trace);
        let result = gate.observe(lease.finish(Ok(()), Some(&|| cancel)));
        if result.is_ok() {
            trace.borrow_mut().push(Trace::Encoder);
        }
        result
    }

    #[test]
    fn timeout_failure_permanently_blocks_later_acquire_copy_and_encoder() {
        let gate = LaneGate::default();
        let first = backend([(2_000, PollStep::Pending), (2_001, PollStep::Complete)]);
        let first_trace = first.trace.clone();
        assert!(guarded_frame(&gate, || first, false).is_err());
        assert_safe_once(&first_trace.borrow());
        assert!(!first_trace.borrow().contains(&Trace::Encoder));
        // 下一次 Acquire 闭包本身不可执行；未获取帧，不需要建立或归还租约。
        let trace = Rc::new(RefCell::new(Vec::new()));
        assert!(guarded_frame(
            &gate,
            || {
                trace.borrow_mut().push(Trace::Acquire);
                backend([])
            },
            false
        )
        .is_err());
        assert!(
            trace.borrow().is_empty(),
            "故障后不允许 Acquire/Copy/编码提交"
        );
    }

    #[test]
    fn healthy_cancellation_keeps_lane_reusable() {
        let gate = LaneGate::default();
        let cancelled = backend([(1, PollStep::Complete)]);
        let trace = cancelled.trace.clone();
        let error = guarded_frame(&gate, || cancelled, true).unwrap_err();
        assert!(error.is::<CaptureCancelled>());
        assert_safe_once(&trace.borrow());
        assert!(!trace.borrow().contains(&Trace::Encoder));
        let next = backend([(1, PollStep::Complete)]);
        let trace = next.trace.clone();
        guarded_frame(&gate, || next, false).unwrap();
        assert_safe_once(&trace.borrow());
        assert!(trace.borrow().contains(&Trace::Encoder));
    }

    #[test]
    fn cancellation_with_release_error_permanently_fails_lane() {
        let gate = LaneGate::default();
        let mut cancelled = backend([(1, PollStep::Complete)]);
        cancelled.release_error = Some(Failure(201).into());
        let trace = cancelled.trace.clone();
        let error = guarded_frame(&gate, || cancelled, true).unwrap_err();
        assert_eq!(
            primary(error.as_ref()).downcast_ref::<Failure>().unwrap().0,
            201
        );
        assert_safe_once(&trace.borrow());
        assert!(gate.check().is_err());
        assert!(!trace.borrow().contains(&Trace::Encoder));
    }

    #[test]
    fn lane_observer_preserves_original_failure_and_cannot_be_reset_by_success() {
        let gate = LaneGate::default();
        let failure: Box<dyn Error> = Box::new(Failure(211));
        let address = std::ptr::from_ref(failure.as_ref());
        let error = gate.observe::<()>(Err(failure)).unwrap_err();
        assert!(std::ptr::eq(address, std::ptr::from_ref(error.as_ref())));
        assert!(gate.check().is_err());
        gate.observe(Ok(())).unwrap();
        assert!(gate.check().is_err());
    }

    #[test]
    fn matching_cancel_text_is_failure_and_closes_lane() {
        let gate = LaneGate::default();
        let error = gate.observe::<()>(Err("采集已取消".into())).unwrap_err();
        assert!(!error.is::<CaptureCancelled>());
        assert!(gate.check().is_err());
    }
}
