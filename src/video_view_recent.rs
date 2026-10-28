//! 最近视频菜单与串行后台配置操作，不在界面线程读写配置或探测媒体路径。
#[path = "video_open_recent.rs"]
mod store;

use super::{button, VideoView};
use crate::video_file_queue::VideoFileQueue;
use gpui::{div, prelude::*, px, rgb, Context};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const MAX_PENDING_ACTIONS: usize = 32;
static ACTIVE_RUNNERS: AtomicUsize = AtomicUsize::new(0);

/// 不依赖窗口或回执任务存活，退出入口可异步等待已接受的配置操作排空。
pub(super) fn has_pending_work() -> bool {
    ACTIVE_RUNNERS.load(Ordering::Acquire) != 0
}

struct RunnerActivity<'a>(&'a AtomicUsize);

impl<'a> RunnerActivity<'a> {
    fn begin(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for RunnerActivity<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

enum Action {
    Load,
    Record(PathBuf),
    Remove(PathBuf),
    Clear,
}

struct QueuedAction {
    sequence: u64,
    action: Action,
}

#[derive(Default)]
struct Work {
    pending: VecDeque<QueuedAction>,
    running: bool,
    active: bool,
    sequence: u64,
    latest: Option<(u64, Vec<PathBuf>)>,
    error: Option<String>,
}

impl Work {
    /// 返回是否需要启动drain线程；上限包含当前执行项，不另藏第33项。
    fn enqueue(&mut self, action: Action) -> Result<bool, String> {
        if matches!(action, Action::Load)
            && self
                .pending
                .back()
                .is_some_and(|queued| matches!(queued.action, Action::Load))
        {
            return Ok(false);
        }
        if self.pending.len() + usize::from(self.active) >= MAX_PENDING_ACTIONS {
            return Err(tr!(
                "最近视频更新繁忙，请稍后重试",
                "Recent videos are busy; try again shortly"
            )
            .into());
        }
        let sequence = self.sequence.checked_add(1).ok_or_else(|| {
            tr!(
                "最近视频请求过多，请重新打开窗口",
                "Too many recent video requests; reopen the viewer"
            )
            .to_string()
        })?;
        self.sequence = sequence;
        self.pending.push_back(QueuedAction { sequence, action });
        let start = !self.running;
        self.running = true;
        Ok(start)
    }

    fn report(&mut self) -> (bool, Option<(u64, Vec<PathBuf>)>, Option<String>) {
        // 新意图未完成时不显示旧结果，也不丢弃最后成功列表；后继操作失败时，
        // 排空后仍应显示已完成的清空/移除结果，并单独报告后继错误。
        let latest = if self
            .latest
            .as_ref()
            .is_some_and(|(sequence, _)| *sequence == self.sequence || !self.running)
        {
            self.latest.take()
        } else {
            None
        };
        (self.running, latest, self.error.take())
    }
}

type SharedWork = Arc<Mutex<Work>>;

fn execute_action(store: &store::VideoRecentStore, action: Action) -> Result<Vec<PathBuf>, String> {
    match action {
        Action::Load => store.load(),
        Action::Record(path) => store.record_opened(&path),
        Action::Remove(path) => store.remove(&path),
        Action::Clear => store.clear(),
    }
    .map_err(|error| error.to_string())
}

/// 线程拥有队列直到清空。VideoView消失只停止UI回执，不撤销已接受的持久化操作。
fn spawn_runner(
    shared: SharedWork,
    mut execute: impl FnMut(Action) -> Result<Vec<PathBuf>, String> + Send + 'static,
) -> std::io::Result<()> {
    // spawn失败会析构闭包，线程panic或正常排空也会析构同一守卫。
    let activity = RunnerActivity::begin(&ACTIVE_RUNNERS);
    std::thread::Builder::new()
        .name("rustshot-video-recent".into())
        .spawn(move || {
            let _activity = activity;
            loop {
                let queued = {
                    let mut work = shared.lock().unwrap_or_else(|error| error.into_inner());
                    let Some(queued) = work.pending.pop_front() else {
                        work.running = false;
                        return;
                    };
                    work.active = true;
                    queued
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute(queued.action)
                }))
                .unwrap_or_else(|_| {
                    Err(tr!("最近视频后台操作失败", "A recent video operation failed").into())
                });
                let mut work = shared.lock().unwrap_or_else(|error| error.into_inner());
                work.active = false;
                match result {
                    Ok(paths) => work.latest = Some((queued.sequence, paths)),
                    // 后面的成功操作不能掩盖前面尚未报告的失败；只保留最近一条错误，仍然有界。
                    Err(error) => work.error = Some(error),
                }
            }
        })
        .map(drop)
}

#[derive(Default)]
pub(super) struct RecentState {
    paths: Vec<PathBuf>,
    visible: bool,
    busy: bool,
    watching: bool,
    applied_sequence: u64,
    work: SharedWork,
}

impl VideoView {
    pub(super) fn record_recent_current(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = &self.path {
            self.enqueue_recent(Action::Record(path.clone()), cx);
        }
    }

    fn enqueue_recent(&mut self, action: Action, cx: &mut Context<Self>) {
        let result = {
            let mut work = self
                .recent
                .work
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            work.enqueue(action).and_then(|start| {
                if start {
                    if let Err(error) = spawn_runner(self.recent.work.clone(), |action| {
                        execute_action(&store::VideoRecentStore::new(), action)
                    }) {
                        // 尚未接受的启动失败不留下永远无法drain的pending项。
                        work.pending.clear();
                        work.running = false;
                        work.sequence -= 1;
                        return Err(error.to_string());
                    }
                }
                Ok(())
            })
        };
        if let Err(error) = result {
            self.notice = Some(error);
            cx.notify();
            return;
        }
        self.recent.busy = true;
        if self.recent.watching {
            cx.notify();
            return;
        }
        self.recent.watching = true;
        let shared = self.recent.work.clone();
        cx.spawn(async move |weak, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let (running, latest, error) = shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .report();
            let keep_watching = weak.update(cx, |view, cx| {
                let mut changed = view.recent.busy != running;
                view.recent.busy = running;
                if let Some((sequence, paths)) = latest {
                    if sequence > view.recent.applied_sequence {
                        view.recent.applied_sequence = sequence;
                        view.recent.paths = paths;
                        changed = true;
                    }
                }
                if let Some(error) = error {
                    view.notice = Some(trf!(
                        "最近视频更新失败：{error}",
                        "Recent videos could not be updated: {error}"
                    ));
                    changed = true;
                }
                if !running {
                    view.recent.watching = false;
                }
                if changed {
                    cx.notify();
                }
                running
            });
            if !matches!(keep_watching, Ok(true)) {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn recent_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        button(
            "video-recent",
            tr!("最近", "Recent"),
            self.recent.visible,
            cx,
        )
        .on_click(cx.listener(|view, _, _, cx| {
            view.recent.visible = !view.recent.visible;
            if view.recent.visible {
                view.show_details = false;
                view.hide_queue();
                view.hide_audio();
                view.hide_gif();
                view.enqueue_recent(Action::Load, cx);
            }
            cx.notify();
        }))
    }

    pub(super) fn hide_recent(&mut self) {
        self.recent.visible = false;
    }

    pub(super) fn recent_visible(&self) -> bool {
        self.recent.visible
    }

    pub(super) fn recent_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.recent.visible {
            return None;
        }
        let p = crate::theme::palette(cx);
        let mut menu = div()
            .id("video-recent-menu")
            .w_full()
            .max_h(px(120.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex_shrink()
            .px(px(16.))
            .pb(px(8.))
            .bg(rgb(p.background))
            .border_b_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(if self.recent.busy {
                                tr!("正在更新最近视频…", "Updating recent videos…")
                            } else {
                                tr!("仅记录成功打开的视频", "Videos opened successfully")
                            }),
                    )
                    .child(
                        button(
                            "video-clear-recent",
                            tr!("清空记录", "Clear list"),
                            false,
                            cx,
                        )
                        .on_click(
                            cx.listener(|view, _, _, cx| view.enqueue_recent(Action::Clear, cx)),
                        ),
                    ),
            );
        if self.recent.paths.is_empty() {
            menu = menu.child(
                div()
                    .py(px(10.))
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(tr!("暂无最近视频", "No recent videos")),
            );
        }
        for (index, path) in self.recent.paths.iter().enumerate() {
            let open_path = path.clone();
            let remove_path = path.clone();
            menu =
                menu.child(
                    div()
                        .id(("video-recent-row", index))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(3.))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(move |style| style.bg(rgb(p.hover)))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if view.prompt_pending || !view.recent.visible {
                                return;
                            }
                            match VideoFileQueue::from_paths([open_path.clone()]) {
                                Ok(queue) => view.replace_queue(queue),
                                Err(error) => view.notice = Some(error),
                            }
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(12.))
                                .child(path.display().to_string()),
                        )
                        .child(button("video-remove-recent", "×", false, cx).on_click(
                            cx.listener(move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.enqueue_recent(Action::Remove(remove_path.clone()), cx);
                            }),
                        )),
                );
        }
        Some(menu.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_json::TestDir;
    use std::sync::mpsc;

    fn media(name: &str) -> PathBuf {
        if cfg!(windows) {
            std::path::Path::new(r"C:\Media").join(name)
        } else {
            std::path::Path::new("/media").join(name)
        }
    }

    #[test]
    fn recent_worker_drains_clear_and_remove_after_ui_owner_is_gone() {
        let directory = TestDir::new("video-recent-drain");
        let store = store::VideoRecentStore::at(directory.join("recent.json"));
        store.record_opened(&media("a.mp4")).unwrap();
        let worker_store = store.clone();
        let ui_work: SharedWork = Arc::default();
        assert!(ui_work
            .lock()
            .unwrap()
            .enqueue(Action::Record(media("a.mp4")))
            .unwrap());
        let (entered, seen) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (finished, completed) = mpsc::channel();
        spawn_runner(ui_work.clone(), move |action| {
            let first = matches!(&action, Action::Record(path) if *path == media("a.mp4"));
            let last = matches!(&action, Action::Record(path) if *path == media("c.mp4"));
            if first {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let result = execute_action(&worker_store, action);
            if last {
                finished
                    .send(result.as_ref().map(|_| ()).map_err(Clone::clone))
                    .unwrap();
            }
            result
        })
        .unwrap();
        seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(has_pending_work());
        {
            let mut work = ui_work.lock().unwrap();
            work.enqueue(Action::Clear).unwrap();
            work.enqueue(Action::Record(media("b.mp4"))).unwrap();
            work.enqueue(Action::Remove(media("b.mp4"))).unwrap();
            work.enqueue(Action::Record(media("c.mp4"))).unwrap();
        }
        // 对应VideoView及其RecentState析构；此后只有后台线程拥有队列。
        drop(ui_work);
        release.send(()).unwrap();
        completed
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(store.load().unwrap(), vec![media("c.mp4")]);
    }

    #[test]
    fn recent_runner_activity_is_released_during_unwind() {
        // 使用局部计数，避免并行worker测试影响“归零”判断。
        let counter = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(|| {
            let _activity = RunnerActivity::begin(&counter);
            assert_eq!(counter.load(Ordering::Acquire), 1);
            panic!("simulated recent runner panic");
        });
        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn recent_queue_limit_includes_the_active_action() {
        let mut work = Work::default();
        for index in 0..MAX_PENDING_ACTIONS {
            assert_eq!(
                work.enqueue(Action::Record(media(&format!("{index}.mp4"))))
                    .unwrap(),
                index == 0
            );
        }
        assert!(work.enqueue(Action::Clear).is_err());
        work.pending.pop_front();
        work.active = true;
        assert!(work.enqueue(Action::Clear).is_err());
        work.active = false;
        assert!(!work.enqueue(Action::Clear).unwrap());
        assert_eq!(work.pending.len(), MAX_PENDING_ACTIONS);
    }

    #[test]
    fn recent_old_load_snapshot_cannot_restore_menu_after_a_new_clear_intent() {
        let mut work = Work::default();
        work.enqueue(Action::Load).unwrap();
        work.latest = Some((1, vec![media("old.mp4")]));
        work.enqueue(Action::Clear).unwrap();
        assert!(work.report().1.is_none());
        work.latest = Some((2, Vec::new()));
        assert_eq!(work.report().1, Some((2, Vec::new())));
        assert!(work.report().1.is_none());
    }

    #[test]
    fn recent_successful_mutation_is_reported_when_a_later_load_fails() {
        for remove in [false, true] {
            let directory = TestDir::new("video-recent-load-failure");
            let store = store::VideoRecentStore::at(directory.join("recent.json"));
            store.record_opened(&media("b.mp4")).unwrap();
            store.record_opened(&media("a.mp4")).unwrap();
            let expected = if remove {
                vec![media("b.mp4")]
            } else {
                Vec::new()
            };
            let shared: SharedWork = Arc::default();
            {
                let mut work = shared.lock().unwrap();
                work.enqueue(if remove {
                    Action::Remove(media("a.mp4"))
                } else {
                    Action::Clear
                })
                .unwrap();
                work.enqueue(Action::Load).unwrap();
            }
            let (entered, started) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            let worker_store = store.clone();
            spawn_runner(shared.clone(), move |action| {
                if matches!(action, Action::Load) {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    return Err("simulated recent load failure".to_string());
                }
                execute_action(&worker_store, action)
            })
            .unwrap();
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(store.load().unwrap(), expected);
            {
                let mut work = shared.lock().unwrap();
                let (running, latest, error) = work.report();
                assert!(running);
                assert!(latest.is_none());
                assert!(error.is_none());
                assert_eq!(work.latest, Some((1, expected.clone())));
            }
            release.send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while shared.lock().unwrap().running {
                assert!(std::time::Instant::now() < deadline, "recent runner未排空");
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut work = shared.lock().unwrap();
            let (running, latest, error) = work.report();
            assert!(!running);
            assert_eq!(latest, Some((1, expected)));
            assert_eq!(error.as_deref(), Some("simulated recent load failure"));
            assert!(work.report().1.is_none());
        }
    }
}
