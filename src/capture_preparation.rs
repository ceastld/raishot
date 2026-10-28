//! 原生首屏就绪后才放行主线程初始化；冻结任务在所有退出路径都会回收。
use std::{
    io,
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
};

pub(super) struct ReadyGate<T>(Option<SyncSender<T>>);

impl<T> ReadyGate<T> {
    pub(super) fn release(&mut self, value: T) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(value);
        }
    }
}

pub(super) struct PreparationTask<T>(Option<JoinHandle<T>>);

impl<T> PreparationTask<T> {
    pub(super) fn join(mut self) -> thread::Result<T> {
        self.0.take().expect("冻结准备任务只回收一次").join()
    }
}

impl<T> Drop for PreparationTask<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            // 初始化失败/取消时也等待原任务释放同源像素与原生资源。
            let _ = task.join();
        }
    }
}

pub(super) fn spawn<T, R>(
    work: impl FnOnce(ReadyGate<T>) -> R + Send + 'static,
) -> io::Result<(Receiver<T>, PreparationTask<R>)>
where
    T: Send + 'static,
    R: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let task = thread::Builder::new()
        .name("rustshot-freeze".into())
        .spawn(move || work(ReadyGate(Some(sender))))?;
    // 无原生 Ready（回退、错误或 panic）时，sender 随任务终态释放。
    // 调用方必须先 join 并核验结果，再初始化 GPUI。
    Ok((receiver, PreparationTask(Some(task))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn native_ready_allows_initialization_while_same_source_is_still_preparing() {
        let (finish, finishing) = mpsc::sync_channel(1);
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        let (ready, task) = spawn(move |mut gate| {
            gate.release(17);
            finishing.recv().unwrap();
            worker_completed.store(true, Ordering::Release);
            23
        })
        .unwrap();
        assert_eq!(ready.recv().ok(), Some(17));
        assert!(!completed.load(Ordering::Acquire));
        finish.send(()).unwrap();
        assert_eq!(task.join().unwrap(), 23);
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn fallback_cannot_release_initialization_before_complete_source() {
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        let (ready, task) = spawn(move |_: ReadyGate<()>| {
            worker_completed.store(true, Ordering::Release);
            23
        })
        .unwrap();
        assert_eq!(ready.recv().ok(), None);
        assert!(completed.load(Ordering::Acquire));
        assert_eq!(task.join().unwrap(), 23);
    }

    #[test]
    fn failure_before_or_after_ready_keeps_typed_cancellation_and_error() {
        for early in [false, true] {
            for cancelled in [false, true] {
                let (ready, task) = spawn(move |mut gate| {
                    if early {
                        gate.release(17);
                    }
                    Err::<(), _>((cancelled, "原来源准备失败".to_owned()))
                })
                .unwrap();
                assert_eq!(ready.recv().ok(), early.then_some(17));
                assert_eq!(
                    task.join().unwrap(),
                    Err((cancelled, "原来源准备失败".to_owned()))
                );
            }
        }
    }

    #[test]
    fn panic_disconnects_gate_and_remains_a_join_failure() {
        let (ready, task) = spawn::<(), ()>(|_| {
            panic!("准备失败");
        })
        .unwrap();
        assert_eq!(ready.recv().ok(), None);
        assert!(task.join().is_err());
    }

    #[test]
    fn early_exit_joins_worker_and_ready_is_only_released_once() {
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        let (ready, task) = spawn(move |mut gate| {
            gate.release(17);
            gate.release(18);
            worker_completed.store(true, Ordering::Release);
        })
        .unwrap();
        assert_eq!(ready.recv().ok(), Some(17));
        drop(task);
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn closed_receiver_does_not_prevent_worker_cleanup() {
        let (release, released) = mpsc::sync_channel(1);
        let (ready, task) = spawn(move |mut gate| {
            released.recv().unwrap();
            gate.release(17);
            23
        })
        .unwrap();
        drop(ready);
        release.send(()).unwrap();
        assert_eq!(task.join().unwrap(), 23);
    }

    #[test]
    fn panic_after_native_ready_remains_a_join_failure() {
        let (ready, task) = spawn::<_, ()>(|mut gate| {
            gate.release(17);
            panic!("首屏后准备失败");
        })
        .unwrap();
        assert_eq!(ready.recv().ok(), Some(17));
        assert!(task.join().is_err());
    }
}
