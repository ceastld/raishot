//! 由显式用户操作启动的输入检测；设备的初始化和释放始终归后台线程所有。
use super::{AudioCapture, AudioSource, CaptureSourceSnapshot};
use crate::{audio_devices::EndpointSelection, media_playback_sync::WorkerExit, Result};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone)]
pub(crate) struct InputCheckSnapshot {
    pub revision: u64,
    pub starting: bool,
    pub sources: Vec<CaptureSourceSnapshot>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

impl InputCheckSnapshot {
    fn starting() -> Self {
        Self {
            revision: 0,
            starting: true,
            sources: Vec::new(),
            warnings: Vec::new(),
            error: None,
        }
    }
}

type SharedSnapshot = Arc<Mutex<InputCheckSnapshot>>;

pub(crate) struct AudioInputCheck {
    stop: Arc<AtomicBool>,
    shared: SharedSnapshot,
    thread: Option<JoinHandle<()>>,
    closed: WorkerExit,
}

impl AudioInputCheck {
    /// 仅显式检测按钮调用；打开设备列表或面板不能启动本会话。
    pub(crate) fn start(
        sources: Vec<AudioSource>,
        system_output: EndpointSelection,
        microphone_input: EndpointSelection,
    ) -> Result<Self> {
        if sources.is_empty() {
            return Err(tr!(
                "请先选择要检测的音频来源",
                "Choose an audio source to check"
            )
            .into());
        }
        Self::spawn_owner(move |stop, shared| {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // AudioCapture 的初始化等待和 Drop join 都只在此 owner 内发生。
            let (capture, warnings) =
                AudioCapture::start_monitoring(&sources, &system_output, &microphone_input)?;
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let source_status = capture.source_status();
            capture.discard();
            publish(&shared, &stop, |snapshot| {
                snapshot.starting = false;
                snapshot.sources = source_status;
                snapshot.warnings = warnings;
            });
            while !stop.load(Ordering::Acquire) {
                std::thread::park_timeout(POLL_INTERVAL);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let source_status = capture.source_status();
                // 检测只保留统计快照，不累积或写出 PCM。
                capture.discard();
                publish(&shared, &stop, |snapshot| snapshot.sources = source_status);
            }
            // capture 在闭包返回前释放，退出回执由外层 guard 最后发布。
            drop(capture);
            Ok(())
        })
    }

    fn spawn_owner(
        owner: impl FnOnce(Arc<AtomicBool>, SharedSnapshot) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(InputCheckSnapshot::starting()));
        let closed = WorkerExit::pending();
        let worker_stop = stop.clone();
        let worker_shared = shared.clone();
        let worker_closed = closed.clone();
        let thread = std::thread::Builder::new()
            .name("rustshot-audio-input-check".into())
            .spawn(move || {
                let _closed = worker_closed.guard();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner(worker_stop.clone(), worker_shared.clone())
                }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(_) => Some(
                        tr!(
                            "输入检测线程意外停止",
                            "Audio input check stopped unexpectedly"
                        )
                        .into(),
                    ),
                };
                if let Some(error) = error {
                    publish(&worker_shared, &worker_stop, |snapshot| {
                        snapshot.starting = false;
                        snapshot.error = Some(error);
                    });
                }
            })?;
        Ok(Self {
            stop,
            shared,
            thread: Some(thread),
            closed,
        })
    }

    pub(crate) fn snapshot(&self) -> InputCheckSnapshot {
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 仅冻结小快照、发停止信号并分离线程；设备收尾不会阻塞调用方。
    pub(crate) fn close(&mut self) {
        {
            let mut snapshot = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !self.stop.swap(true, Ordering::AcqRel) {
                snapshot.starting = false;
                snapshot.revision = snapshot.revision.wrapping_add(1);
            }
        }
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            drop(thread);
        }
    }

    /// 为真才表示 owner 已释放采集对象；请求 close 本身不会提前置真。
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.is_exited()
    }

    pub(crate) fn closed_probe(&self) -> WorkerExit {
        self.closed.clone()
    }
}

impl Drop for AudioInputCheck {
    fn drop(&mut self) {
        self.close();
    }
}

fn publish(
    shared: &SharedSnapshot,
    stop: &AtomicBool,
    update: impl FnOnce(&mut InputCheckSnapshot),
) {
    let mut snapshot = shared.lock().unwrap_or_else(|error| error.into_inner());
    if !stop.load(Ordering::Acquire) {
        update(&mut snapshot);
        snapshot.revision = snapshot.revision.wrapping_add(1);
    }
}

#[cfg(test)]
#[path = "audio_input_check_probe.rs"]
mod native_probe;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Instant};

    fn wait_closed(probe: &WorkerExit) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !probe.is_exited() && Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(1));
        }
        assert!(probe.is_exited(), "纯测试 owner 未退出");
    }

    #[test]
    fn audio_input_check_drop_detaches_until_owner_resources_are_released() {
        struct Owned(Arc<AtomicBool>);
        impl Drop for Owned {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let worker_released = released.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let session = AudioInputCheck::spawn_owner(move |_, _| {
            let _owned = Owned(worker_released);
            let _ = ready_tx.send(());
            let _ = release_rx.recv();
            Ok(())
        })
        .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let probe = session.closed_probe();
        let starting = session.snapshot();
        assert!(starting.starting && starting.sources.is_empty() && starting.error.is_none());
        assert!(!session.is_closed());
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(session);
            let _ = dropped_tx.send(());
        });
        let detached = dropped_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        let premature_exit = probe.is_exited();
        let _ = release_tx.send(());
        dropper.join().unwrap();
        wait_closed(&probe);
        assert!(detached, "GUI Drop 等待了被阻塞的 owner");
        assert!(!premature_exit, "退出回执早于 owner 资源释放");
        assert!(released.load(Ordering::Acquire));
    }

    #[test]
    fn audio_input_check_close_freezes_old_session_without_affecting_new_session() {
        let (old_ready_tx, old_ready_rx) = mpsc::channel();
        let (old_release_tx, old_release_rx) = mpsc::channel();
        let mut old = AudioInputCheck::spawn_owner(move |stop, shared| {
            publish(&shared, &stop, |snapshot| {
                snapshot.starting = false;
                snapshot.warnings.push("old".into());
            });
            let _ = old_ready_tx.send(());
            let _ = old_release_rx.recv();
            publish(&shared, &stop, |snapshot| {
                snapshot.warnings.push("late old".into())
            });
            Ok(())
        })
        .unwrap();
        old_ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        old.close();
        let frozen = old.snapshot();
        let (new_ready_tx, new_ready_rx) = mpsc::channel();
        let mut new = AudioInputCheck::spawn_owner(move |stop, shared| {
            publish(&shared, &stop, |snapshot| {
                snapshot.starting = false;
                snapshot.warnings.push("new".into());
            });
            let _ = new_ready_tx.send(());
            while !stop.load(Ordering::Acquire) {
                std::thread::park();
            }
            Ok(())
        })
        .unwrap();
        new_ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let _ = old_release_tx.send(());
        wait_closed(&old.closed_probe());
        assert_eq!(old.snapshot().revision, frozen.revision);
        assert_eq!(old.snapshot().warnings, ["old"]);
        assert!(!old.snapshot().starting);
        assert_eq!(new.snapshot().warnings, ["new"]);
        new.close();
        wait_closed(&new.closed_probe());
    }

    #[test]
    fn audio_input_check_panic_publishes_error_and_empty_sources_never_start_backend() {
        let session = AudioInputCheck::spawn_owner(|_, _| -> Result<()> {
            panic!("模拟检测 owner 错误");
        })
        .unwrap();
        wait_closed(&session.closed_probe());
        let snapshot = session.snapshot();
        assert!(!snapshot.starting && snapshot.error.is_some() && snapshot.revision > 0);
        assert!(AudioInputCheck::start(
            Vec::new(),
            EndpointSelection::Default,
            EndpointSelection::Default
        )
        .is_err());
    }
}
