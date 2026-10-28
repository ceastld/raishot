//! 视频打开前的单槽后台预检；不读取媒体正文，不启动播放器，也不枚举相邻文件。
use crate::video_file_queue::VideoFileQueue;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
};

/// 不同来源都复用同一有界队列合同，空拖入不应替换已有队列。
pub(crate) fn queue_from_drop(
    paths: &gpui::ExternalPaths,
) -> Result<Option<VideoFileQueue>, String> {
    if paths.paths().is_empty() {
        return Ok(None);
    }
    VideoFileQueue::from_paths(paths.paths().iter().cloned()).map(Some)
}

#[derive(Clone, Debug)]
pub(crate) struct VideoOpenTicket(Arc<()>);

impl VideoOpenTicket {
    pub(crate) fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone)]
struct Request {
    ticket: VideoOpenTicket,
    index: usize,
    path: PathBuf,
}

pub(crate) struct VideoOpenCompletion {
    pub(crate) ticket: VideoOpenTicket,
    pub(crate) index: usize,
    pub(crate) path: PathBuf,
    /// Ok只表示本地普通文件可尝试读取，不表示容器/编解码或声音已验证。
    pub(crate) result: Result<(), String>,
}

#[derive(Default)]
struct Mailbox {
    current: Option<Request>,
    pending: Option<Request>,
    completed: Option<VideoOpenCompletion>,
    stopped: bool,
}

/// 一条工作线程、一个待处理请求和一个回执。快速切换不会排队整张播放列表。
/// UI在调用request/cancel时同步取消旧帧输出；回执须再次核对所持ticket后才能换Player。
pub(crate) struct VideoOpener {
    shared: Arc<(Mutex<Mailbox>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl VideoOpener {
    pub(crate) fn new() -> Result<Self, String> {
        Self::spawn_with(crate::image_collection::ensure_local_file)
    }

    fn spawn_with(
        check: impl Fn(&Path) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rustshot-video-file-check".into())
            .spawn(move || loop {
                let request = {
                    let (lock, signal) = &*worker;
                    let mut state = lock.lock().unwrap();
                    while !state.stopped && state.pending.is_none() {
                        state = signal.wait(state).unwrap();
                    }
                    if state.stopped {
                        return;
                    }
                    state.pending.take().expect("pending file request")
                };
                // 不持锁做文件系统查询；请求可在慢网络元数据返回前被取消/替换。
                let result = check(&request.path);
                let (lock, signal) = &*worker;
                let mut state = lock.lock().unwrap();
                if !state.stopped
                    && state
                        .current
                        .as_ref()
                        .is_some_and(|current| current.ticket.same_request(&request.ticket))
                {
                    state.completed = Some(VideoOpenCompletion {
                        ticket: request.ticket,
                        index: request.index,
                        path: request.path,
                        result,
                    });
                    signal.notify_all();
                }
            })
            .map_err(|error| {
                trf!(
                    "无法启动视频文件检查：{error}",
                    "Cannot start video file checks: {error}"
                )
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// 仅接受队列已锚定的有效词法绝对路径；不重新解释驱动器相对路径。
    pub(crate) fn request(
        &mut self,
        index: usize,
        path: PathBuf,
    ) -> Result<VideoOpenTicket, String> {
        if !path.is_absolute() || path.as_os_str().to_string_lossy().contains('\0') {
            return Err(tr!(
                "视频打开请求需要有效的绝对路径",
                "A video open request needs a valid absolute path"
            )
            .into());
        }
        if self
            .thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
        {
            return Err(worker_stopped());
        }
        let ticket = VideoOpenTicket(Arc::new(()));
        let request = Request {
            ticket: ticket.clone(),
            index,
            path,
        };
        let (lock, signal) = &*self.shared;
        let mut state = lock.lock().unwrap();
        if state.stopped {
            return Err(worker_stopped());
        }
        state.current = Some(request.clone());
        state.pending = Some(request);
        state.completed = None;
        signal.notify_one();
        Ok(ticket)
    }

    pub(crate) fn cancel(&mut self) {
        let (lock, _) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.current = None;
        state.pending = None;
        state.completed = None;
    }

    /// 不阻塞。消费回执后仍保留当前请求身份；新请求/取消会使旧ticket失效。
    pub(crate) fn poll(&mut self) -> Option<VideoOpenCompletion> {
        let (lock, _) = &*self.shared;
        let mut state = lock.lock().unwrap();
        if let Some(completed) = state.completed.take() {
            return Some(completed);
        }
        // 后台意外退出也必须给UI终态，不能让“正在打开”永久停住。
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            let request = state.current.take()?;
            state.pending = None;
            return Some(VideoOpenCompletion {
                ticket: request.ticket,
                index: request.index,
                path: request.path,
                result: Err(worker_stopped()),
            });
        }
        None
    }
}

impl Drop for VideoOpener {
    fn drop(&mut self) {
        let (lock, signal) = &*self.shared;
        {
            let mut state = lock.lock().unwrap();
            state.stopped = true;
            state.current = None;
            state.pending = None;
            state.completed = None;
            signal.notify_all();
        }
        // 元数据系统调用不可中途取消，不在关闭UI时join；返回后线程不再发布或读下一项。
        self.thread.take();
    }
}

fn worker_stopped() -> String {
    tr!(
        "视频文件检查已退出，请重新打开查看窗口",
        "Video file checks stopped; reopen the viewer"
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    fn path(name: &str) -> PathBuf {
        if cfg!(windows) {
            Path::new(r"C:\Media").join(name)
        } else {
            Path::new("/media").join(name)
        }
    }

    fn await_completion(opener: &VideoOpener) {
        let (lock, signal) = &*opener.shared;
        let state = lock.lock().unwrap();
        let (state, timeout) = signal
            .wait_timeout_while(state, Duration::from_secs(2), |state| {
                state.completed.is_none()
            })
            .unwrap();
        assert!(
            !timeout.timed_out() || state.completed.is_some(),
            "file check did not complete"
        );
    }

    #[test]
    fn only_latest_explicit_request_is_published_and_middle_request_is_replaced() {
        let (entered, seen) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let checked = Arc::new(Mutex::new(Vec::new()));
        let worker_checked = checked.clone();
        let mut opener = VideoOpener::spawn_with(move |path| {
            worker_checked.lock().unwrap().push(path.to_path_buf());
            if path.ends_with("first.mp4") {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            Ok(())
        })
        .unwrap();
        let first = opener.request(0, path("first.mp4")).unwrap();
        seen.recv_timeout(Duration::from_secs(2)).unwrap();
        opener.request(1, path("middle.mp4")).unwrap();
        let last = opener.request(2, path("last.mp4")).unwrap();
        assert!(!first.same_request(&last));
        release.send(()).unwrap();
        await_completion(&opener);
        let completion = opener.poll().unwrap();
        assert!(completion.ticket.same_request(&last));
        assert_eq!(completion.index, 2);
        assert_eq!(completion.path, path("last.mp4"));
        assert_eq!(completion.result, Ok(()));
        assert!(opener.poll().is_none());
        assert_eq!(
            *checked.lock().unwrap(),
            vec![path("first.mp4"), path("last.mp4")]
        );
    }

    #[test]
    fn cancel_discards_pending_and_completed_work_without_navigating() {
        let mut opener = VideoOpener::spawn_with(|_| Err("file not local".into())).unwrap();
        opener.request(7, path("offline.mp4")).unwrap();
        await_completion(&opener);
        opener.cancel();
        assert!(opener.poll().is_none());
        let ticket = opener.request(3, path("other.mp4")).unwrap();
        await_completion(&opener);
        let completion = opener.poll().unwrap();
        assert!(completion.ticket.same_request(&ticket));
        assert_eq!(completion.index, 3);
        assert_eq!(completion.result, Err("file not local".into()));
    }

    #[test]
    fn relative_requests_are_rejected_before_worker_checks() {
        let mut opener =
            VideoOpener::spawn_with(|_| panic!("invalid request must not reach metadata checks"))
                .unwrap();
        assert!(opener.request(0, PathBuf::from("clip.mp4")).is_err());
        assert!(opener.request(0, path("bad\0name.mp4")).is_err());
        assert!(opener.poll().is_none());
    }

    #[test]
    fn cancel_in_flight_clears_pending_work_and_never_publishes_the_old_result() {
        let (entered, seen) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let checked = Arc::new(Mutex::new(Vec::new()));
        let worker_checked = checked.clone();
        let mut opener = VideoOpener::spawn_with(move |path| {
            worker_checked.lock().unwrap().push(path.to_path_buf());
            if path.ends_with("blocked.mp4") {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            Ok(())
        })
        .unwrap();
        let old = opener.request(0, path("blocked.mp4")).unwrap();
        seen.recv_timeout(Duration::from_secs(2)).unwrap();
        opener.request(1, path("canceled-pending.mp4")).unwrap();
        opener.cancel();
        assert!(opener.poll().is_none());
        let current = opener.request(2, path("current.mp4")).unwrap();
        release.send(()).unwrap();
        await_completion(&opener);
        let completion = opener.poll().unwrap();
        assert!(!completion.ticket.same_request(&old));
        assert!(completion.ticket.same_request(&current));
        assert_eq!(
            *checked.lock().unwrap(),
            vec![path("blocked.mp4"), path("current.mp4")]
        );
        assert!(opener.poll().is_none());
    }

    #[test]
    fn empty_platform_drop_does_not_replace_the_current_queue() {
        assert!(queue_from_drop(&gpui::ExternalPaths::default())
            .unwrap()
            .is_none());
    }
}
