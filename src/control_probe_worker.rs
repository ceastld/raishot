//! 一个截图会话一个几何查询线程；COM/UIA 及缓存始终留在该 MTA 线程。
use crate::{
    control_selection::{pack_probe_point, ControlPath, ControlProbeSession, ProbeCancellation},
    geometry::{Rect, ScreenRect},
    selection_assist::WindowTarget,
};
use futures::channel::oneshot;
use parking_lot::Mutex;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
};

pub(crate) type ProbeResult = std::result::Result<ControlPath, String>;

struct Request {
    canvas: ScreenRect,
    target: WindowTarget,
    point: (i32, i32),
    cancellation: ProbeCancellation,
    answer: oneshot::Sender<ProbeResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedPoint {
    canvas: ScreenRect,
    hwnd: usize,
    process_id: u32,
    thread_id: u32,
    rect: Rect,
    global_bounds: (i32, i32, i32, i32),
    point: (i32, i32),
}
impl ObservedPoint {
    fn new(canvas: ScreenRect, target: &WindowTarget, point: (i32, i32)) -> Self {
        Self {
            canvas,
            hwnd: target.hwnd,
            process_id: target.process_id,
            thread_id: target.thread_id,
            rect: target.rect,
            global_bounds: (
                target.global_bounds.left,
                target.global_bounds.top,
                target.global_bounds.right,
                target.global_bounds.bottom,
            ),
            point,
        }
    }

    fn same_context(&self, other: &Self) -> bool {
        self.canvas == other.canvas
            && self.hwnd == other.hwnd
            && self.process_id == other.process_id
            && self.thread_id == other.thread_id
            && self.rect == other.rect
            && self.global_bounds == other.global_bounds
    }
}

#[derive(Default)]
struct LatestObservation {
    initialized: bool,
    point: Option<ObservedPoint>,
}

pub(crate) struct ControlProbeWorker {
    sender: mpsc::Sender<Request>,
    latest: Mutex<LatestObservation>,
    generation: Arc<AtomicU64>,
    latest_point: Arc<AtomicU64>,
}

impl Drop for ControlProbeWorker {
    fn drop(&mut self) {
        // 会话结束使在途及排队请求合作取消；不等待或强行中断 provider。
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl ControlProbeWorker {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("rustshot-control-probe".into())
            .spawn(move || {
                let mut session = None;
                for request in receiver {
                    let started = std::time::Instant::now();
                    crate::uia_log::uia_log!(
                        "probe 开始 point={:?} hwnd={:#x} pid={} 窗口={:?} title={:?}",
                        request.point,
                        request.target.hwnd,
                        request.target.process_id,
                        request.target.rect,
                        request.target.title
                    );
                    let result = request
                        .cancellation
                        .check_current()
                        .map_err(|error| error.to_string())
                        .and_then(|()| {
                            match session
                                .get_or_insert_with(ControlProbeSession::new)
                                .as_mut()
                            {
                                Ok(session) => session
                                    .probe_observed(
                                        request.canvas,
                                        request.target,
                                        request.point,
                                        request.cancellation,
                                    )
                                    .map_err(|error| error.to_string()),
                                Err(error) => Err(error.to_string()),
                            }
                        });
                    match &result {
                        Ok(path) => crate::uia_log::uia_log!(
                            "probe 完成 {:.1}ms state={:?} complete={} terminal={} 层数={} 叶={:?}",
                            started.elapsed().as_secs_f64() * 1000.,
                            path.state,
                            path.complete,
                            path.terminal,
                            path.targets.len(),
                            path.targets.first().map(|target| target.rect)
                        ),
                        Err(error) => crate::uia_log::uia_log!(
                            "probe 失败 {:.1}ms：{error}",
                            started.elapsed().as_secs_f64() * 1000.
                        ),
                    }
                    // 初始化失败不能把整次截图永久绑定到一个失败的 COM 会话。
                    if session.as_ref().is_some_and(|session| session.is_err()) {
                        session = None;
                    }
                    let _ = request.answer.send(result);
                }
                // receiver 关闭后在原线程释放 COM；不在界面线程 join provider。
            })?;
        Ok(Self {
            sender,
            latest: Mutex::new(LatestObservation::default()),
            generation: Arc::new(AtomicU64::new(0)),
            latest_point: Arc::new(AtomicU64::new(0)),
        })
    }

    /// point 为 Windows 全局物理坐标；在在途查询期间也持续发布最新点。
    pub(crate) fn observe(
        &self,
        canvas: ScreenRect,
        target: Option<&WindowTarget>,
        point: (i32, i32),
    ) {
        let point = target.map(|target| ObservedPoint::new(canvas, target, point));
        let mut latest = self.latest.lock();
        let same_context = match (&latest.point, &point) {
            (Some(previous), Some(current)) => previous.same_context(current),
            (None, None) => true,
            _ => false,
        };
        if !latest.initialized || !same_context {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(point) = &point {
            self.latest_point
                .store(pack_probe_point(point.point), Ordering::Release);
        }
        latest.initialized = true;
        latest.point = point;
    }

    pub(crate) fn cancel(&self) {
        let mut latest = self.latest.lock();
        latest.initialized = true;
        latest.point = None;
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn probe(
        &self,
        canvas: ScreenRect,
        target: WindowTarget,
        point: (i32, i32),
    ) -> impl Future<Output = ProbeResult> + Send + 'static {
        let requested = ObservedPoint::new(canvas, &target, point);
        let cancellation = {
            let mut latest = self.latest.lock();
            // 首次创建 worker 发生在请求内；只补这个首次点，旧请求不能覆盖观察。
            if !latest.initialized {
                latest.initialized = true;
                latest.point = Some(requested.clone());
                self.generation.fetch_add(1, Ordering::AcqRel);
                self.latest_point
                    .store(pack_probe_point(point), Ordering::Release);
            }
            let generation = self.generation.load(Ordering::Acquire);
            let expected = if latest
                .point
                .as_ref()
                .is_some_and(|latest| latest.same_context(&requested))
            {
                generation
            } else {
                generation.wrapping_sub(1)
            };
            ProbeCancellation::new(self.generation.clone(), expected, self.latest_point.clone())
        };
        let (answer, receiver) = oneshot::channel();
        let submitted = self.sender.send(Request {
            canvas,
            target,
            point,
            cancellation,
            answer,
        });
        // 只把回答的 Future 交给界面；等待不占 GPUI 后台线程，也不持有工作器。
        async move {
            submitted.map_err(|_| "控件查询会话已关闭".to_string())?;
            receiver
                .await
                .map_err(|_| "控件查询线程未返回结果".to_string())?
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::geometry::Rect;
    use futures::{executor::block_on, task::noop_waker};
    use std::task::{Context, Poll};
    use windows::Win32::Foundation::RECT;

    fn transport() -> (ControlProbeWorker, mpsc::Receiver<Request>) {
        let (sender, receiver) = mpsc::channel();
        (
            ControlProbeWorker {
                sender,
                latest: Mutex::new(LatestObservation::default()),
                generation: Arc::new(AtomicU64::new(0)),
                latest_point: Arc::new(AtomicU64::new(0)),
            },
            receiver,
        )
    }

    fn sample() -> (ScreenRect, WindowTarget) {
        let canvas = ScreenRect {
            left: -100,
            top: 0,
            width: 200,
            height: 100,
        };
        let target = WindowTarget {
            rect: Rect::full(200, 100),
            global_bounds: RECT {
                left: -100,
                top: 0,
                right: 100,
                bottom: 100,
            },
            title: String::new(),
            hwnd: 1,
            process_id: 2,
            thread_id: 3,
        };
        (canvas, target)
    }

    #[test]
    fn waiting_reply_is_pending_and_does_not_hold_worker_sender() {
        let (worker, requests) = transport();
        let (canvas, target) = sample();
        let mut reply = Box::pin(worker.probe(canvas, target, (0, 1)));
        let request = requests.try_recv().unwrap();
        assert_eq!(request.point, (0, 1));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(reply.as_mut().poll(&mut cx), Poll::Pending));
        drop(worker);
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(request.answer.send(Err("provider finished".into())).is_ok());
        assert_eq!(block_on(reply).unwrap_err(), "provider finished");
    }

    #[test]
    fn dropped_worker_invalidates_inflight_and_queued_requests_without_joining_provider() {
        let (worker, requests) = transport();
        let (canvas, target) = sample();
        let inflight_reply = worker.probe(canvas, target.clone(), (0, 1));
        let inflight = requests.try_recv().unwrap();
        let queued_reply = worker.probe(canvas, target, (2, 3));
        assert!(inflight.cancellation.check_current().is_ok());

        // 持有真实在途请求，不回复也不释放；退出仍能完成，不能 join 它。
        drop(worker);
        assert!(inflight.cancellation.check_current().is_err());
        let queued = requests.try_recv().unwrap();
        assert!(queued.cancellation.check_current().is_err());
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));

        for request in [inflight, queued] {
            let error = request
                .cancellation
                .check_current()
                .unwrap_err()
                .to_string();
            assert!(request.answer.send(Err(error)).is_ok());
        }
        assert_eq!(
            block_on(inflight_reply).unwrap_err(),
            "控件查询已被最新鼠标位置取消"
        );
        assert_eq!(
            block_on(queued_reply).unwrap_err(),
            "控件查询已被最新鼠标位置取消"
        );
    }

    #[test]
    fn abandoned_reply_does_not_stop_following_request() {
        let (worker, requests) = transport();
        let (canvas, target) = sample();
        let abandoned = worker.probe(canvas, target.clone(), (0, 1));
        drop(abandoned);
        let request = requests.try_recv().unwrap();
        assert!(request.answer.send(Err("obsolete reply".into())).is_err());
        let current = worker.probe(canvas, target, (2, 3));
        let request = requests.try_recv().unwrap();
        assert_eq!(request.point, (2, 3));
        assert!(request.answer.send(Err("current reply".into())).is_ok());
        assert_eq!(block_on(current).unwrap_err(), "current reply");
    }

    #[test]
    fn closed_request_queue_or_reply_returns_error() {
        let (worker, requests) = transport();
        let (canvas, target) = sample();
        drop(requests);
        assert_eq!(
            block_on(worker.probe(canvas, target.clone(), (0, 1))).unwrap_err(),
            "控件查询会话已关闭"
        );
        let (worker, requests) = transport();
        let reply = worker.probe(canvas, target, (0, 1));
        drop(requests.try_recv().unwrap());
        assert_eq!(block_on(reply).unwrap_err(), "控件查询线程未返回结果");
    }

    #[test]
    fn same_window_moves_and_old_submissions_do_not_overwrite_latest_point() {
        let (worker, requests) = transport();
        let (canvas, target) = sample();
        worker.observe(canvas, Some(&target), (0, 1));
        let old_reply = worker.probe(canvas, target.clone(), (0, 1));
        let old = requests.try_recv().unwrap();
        assert!(old.cancellation.check_current().is_ok());
        worker.observe(canvas, Some(&target), (2, 3));
        assert!(old.cancellation.check_current().is_ok());
        let obsolete_reply = worker.probe(canvas, target.clone(), (0, 1));
        let obsolete = requests.try_recv().unwrap();
        assert!(obsolete.cancellation.check_current().is_ok());
        assert_eq!(
            worker.latest_point.load(Ordering::Acquire),
            pack_probe_point((2, 3))
        );
        let current_reply = worker.probe(canvas, target, (2, 3));
        let current = requests.try_recv().unwrap();
        assert!(current.cancellation.check_current().is_ok());
        let mut other_window = current.target.clone();
        other_window.hwnd += 1;
        worker.observe(canvas, Some(&other_window), (2, 3));
        assert!(old.cancellation.check_current().is_err());
        assert!(obsolete.cancellation.check_current().is_err());
        assert!(current.cancellation.check_current().is_err());
        drop((old_reply, obsolete_reply, current_reply));
    }

    #[test]
    fn repeated_observation_stays_current_but_window_identity_or_cancel_invalidates() {
        let (worker, requests) = transport();
        let (canvas, mut target) = sample();
        let reply = worker.probe(canvas, target.clone(), (0, 1));
        let request = requests.try_recv().unwrap();
        worker.observe(canvas, Some(&target), (0, 1));
        assert!(request.cancellation.check_current().is_ok());
        target.process_id += 1;
        worker.observe(canvas, Some(&target), (0, 1));
        assert!(request.cancellation.check_current().is_err());
        let current_reply = worker.probe(canvas, target.clone(), (0, 1));
        let current = requests.try_recv().unwrap();
        assert!(current.cancellation.check_current().is_ok());
        worker.cancel();
        assert!(current.cancellation.check_current().is_err());
        let after_cancel_reply = worker.probe(canvas, target.clone(), (0, 1));
        let after_cancel = requests.try_recv().unwrap();
        assert!(after_cancel.cancellation.check_current().is_err());
        worker.observe(canvas, Some(&target), (0, 1));
        let resumed_reply = worker.probe(canvas, target, (0, 1));
        let resumed = requests.try_recv().unwrap();
        assert!(resumed.cancellation.check_current().is_ok());
        worker.observe(canvas, None, (0, 1));
        assert!(resumed.cancellation.check_current().is_err());
        drop((reply, current_reply, after_cancel_reply, resumed_reply));
    }
}
