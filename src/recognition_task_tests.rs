use super::*;
use std::sync::{
    atomic::AtomicUsize,
    mpsc::{self, Receiver, Sender},
    Barrier,
};
use std::thread;

const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct CountingRun {
    calls: AtomicUsize,
}

impl RunCancellation for CountingRun {
    fn terminate(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

fn count(run: &CountingRun) -> usize {
    run.calls.load(Ordering::Acquire)
}

fn assert_cancelled<T>(result: Result<T>) {
    match result {
        Err(error) => assert!(is_cancelled(error.as_ref()), "必须保留专门取消类型"),
        Ok(_) => panic!("取消任务不应继续成功"),
    }
}

#[derive(Debug)]
struct ControlFailure {
    identity: Arc<()>,
}

impl fmt::Display for ControlFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("终止控制真实失败")
    }
}

impl Error for ControlFailure {}

struct FailingRun {
    calls: AtomicUsize,
    identity: Arc<()>,
}

impl RunCancellation for FailingRun {
    fn terminate(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(ControlFailure {
            identity: self.identity.clone(),
        }
        .into())
    }
}

struct LockCheckingRun {
    task: TaskToken,
    unlocked: AtomicBool,
}

impl RunCancellation for LockCheckingRun {
    fn terminate(&self) -> Result<()> {
        let registration = self.task.state.active.try_lock();
        self.unlocked
            .store(registration.is_some(), Ordering::Release);
        assert!(registration.is_some(), "原生终止调用不得持注册锁");
        Ok(())
    }
}

struct BlockingRun {
    calls: AtomicUsize,
    entered: Sender<()>,
    released: Mutex<Receiver<()>>,
}

impl RunCancellation for BlockingRun {
    fn terminate(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.send(())?;
        self.released.lock().recv_timeout(DEADLINE)?;
        Ok(())
    }
}

#[test]
fn owner_drop_cancels_its_live_worker_and_registered_run() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let run = Arc::new(CountingRun::default());
    let registration = task.register(run.clone()).expect("首次注册应成功");

    drop(owner);

    assert_cancelled(task.checkpoint());
    assert_eq!(count(&run), 1, "Owner 释放应终止本任务活动推理");
    drop(registration);
}

#[test]
fn worker_clone_and_drop_do_not_request_cancellation() {
    let owner = CancelOwner::default();
    let task = owner.token();
    let first = task.clone();
    let second = first.clone();
    drop(first);
    drop(second);
    drop(owner.token());

    task.checkpoint().expect("工作副本析构不能取消 Owner");
    assert!(!task.is_cancelled());
    assert!(owner.cancel().expect("首次取消应成功"));
    assert_cancelled(task.checkpoint());
}

#[test]
fn closing_owner_a_does_not_cancel_owner_b() {
    let owner_a = CancelOwner::new();
    let owner_b = CancelOwner::new();
    let task_a = owner_a.token();
    let task_b = owner_b.token();
    let run_a = Arc::new(CountingRun::default());
    let run_b = Arc::new(CountingRun::default());
    let registration_a = task_a.register(run_a.clone()).expect("A 可注册");
    let registration_b = task_b.register(run_b.clone()).expect("B 可注册");

    drop(owner_a);

    assert_cancelled(task_a.checkpoint());
    task_b.checkpoint().expect("关闭 A 不得取消 B");
    assert_eq!(count(&run_a), 1);
    assert_eq!(count(&run_b), 0);
    drop(registration_a);
    drop(registration_b);
}

#[test]
fn repeated_cancel_requests_terminate_only_once() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let run = Arc::new(CountingRun::default());
    let registration = task.register(run.clone()).expect("可注册");

    assert!(owner.cancel().expect("首次取消应成功"));
    assert!(!owner.cancel().expect("重复取消应返回 false"));
    assert!(!owner.cancel().expect("第三次取消应返回 false"));
    drop(owner);

    assert_eq!(count(&run), 1, "重复取消和析构不得重复原生终止");
    assert_cancelled(task.checkpoint());
    drop(registration);
}

#[test]
fn registering_after_cancel_is_typed_and_never_starts_run() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let run = Arc::new(CountingRun::default());
    assert!(owner.cancel().expect("没有活动推理时也可取消"));

    assert_cancelled(task.register(run.clone()));

    assert_eq!(count(&run), 0, "被拒绝的注册没有活动推理需终止");
    assert!(task
        .state
        .active
        .try_lock()
        .expect("注册锁应释放")
        .is_none());
}

#[test]
fn overlapping_registration_is_real_error_and_preserves_active_target() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let first = Arc::new(CountingRun::default());
    let second = Arc::new(CountingRun::default());
    let registration = task.register(first.clone()).expect("首次注册应成功");

    match task.register(second.clone()) {
        Err(error) => {
            assert!(!is_cancelled(error.as_ref()), "重叠注册不属于取消");
            assert_eq!(error.to_string(), "当前 OCR 请求已有活动推理选项");
        }
        Ok(_) => panic!("同任务不能同时覆盖活动选项"),
    }
    task.checkpoint().expect("注册错误不得取消任务");
    assert!(owner.cancel().expect("现有活动目标仍可取消"));

    assert_eq!(count(&first), 1);
    assert_eq!(count(&second), 0);
    drop(registration);
}

#[test]
fn dropping_completed_registration_detaches_without_cancelling_task() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let completed = Arc::new(CountingRun::default());
    let next = Arc::new(CountingRun::default());
    let registration = task.register(completed.clone()).expect("首次注册应成功");

    drop(registration);
    task.checkpoint().expect("Run 返回解绑不能取消任务");
    let next_registration = task.register(next.clone()).expect("同任务后续阶段可注册");
    assert!(owner.cancel().expect("仅取消当前阶段"));

    assert_eq!(count(&completed), 0);
    assert_eq!(count(&next), 1);
    drop(next_registration);
}

#[test]
fn stale_registration_drop_cannot_detach_new_arc_on_same_token() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let old = Arc::new(CountingRun::default());
    let current = Arc::new(CountingRun::default());
    let registration = task.register(old.clone()).expect("首次注册应成功");
    // 定向验证 ptr_eq 防御分支：旧完成持有自身目标，不得清除后来目标。
    let stale = RunRegistration {
        token: task.clone(),
        control: old.clone(),
    };
    drop(registration);
    let current_registration = task.register(current.clone()).expect("后续阶段可注册");

    drop(stale);
    assert!(owner.cancel().expect("新目标仍应绑定"));

    assert_eq!(count(&old), 0);
    assert_eq!(count(&current), 1);
    drop(current_registration);
}

#[test]
fn termination_callback_runs_outside_registration_lock() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let run = Arc::new(LockCheckingRun {
        task: task.clone(),
        unlocked: AtomicBool::new(false),
    });
    let registration = task.register(run.clone()).expect("可注册");

    assert!(owner.cancel().expect("终止回调应成功"));

    assert!(run.unlocked.load(Ordering::Acquire));
    drop(registration);
}

#[test]
fn termination_failure_preserves_original_error_and_requested_state() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let identity = Arc::new(());
    let run = Arc::new(FailingRun {
        calls: AtomicUsize::new(0),
        identity: identity.clone(),
    });
    let registration = task.register(run.clone()).expect("可注册");

    let error = owner.cancel().expect_err("原生控制失败应原样返回");
    assert!(!is_cancelled(error.as_ref()), "不能把真实错误转换为取消");
    let original = error
        .downcast_ref::<ControlFailure>()
        .expect("真实错误类型不得被抹平");
    assert!(Arc::ptr_eq(&original.identity, &identity));
    assert_cancelled(task.checkpoint());
    assert!(!owner.cancel().expect("已接受请求不重复调用失败目标"));
    assert_eq!(run.calls.load(Ordering::Acquire), 1);
    drop(registration);
}

#[test]
fn arbitrary_cancel_words_do_not_qualify_as_typed_cancellation() {
    let strings = ["OCR 任务已取消", "cancelled", "已取消：加载真实失败"];
    for text in strings {
        let error: Box<dyn Error> = text.into();
        assert!(!is_cancelled(error.as_ref()), "字符串不能冒充专门取消");
    }
    let error = std::io::Error::new(std::io::ErrorKind::Interrupted, "OCR 任务已取消");
    assert!(!is_cancelled(&error), "其它错误类别仍须保留");
    assert!(is_cancelled(&OcrCancelled));
}

#[test]
fn queued_cancelled_task_exits_while_engine_remains_locked() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let engine = Arc::new(Mutex::new(()));
    let held_engine = engine.lock();
    let initialized = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker_engine = engine.clone();
    let worker_initialized = initialized.clone();
    let worker = thread::spawn(move || {
        task.checkpoint().expect("排队前任务应可用");
        entered_tx.send(()).expect("通知排队边界");
        let cancelled = match wait_for_lock(&worker_engine, &task) {
            Err(error) => is_cancelled(error.as_ref()),
            Ok(_guard) => {
                worker_initialized.store(true, Ordering::Release);
                false
            }
        };
        finished_tx.send(cancelled).expect("通知排队结果");
    });

    entered_rx
        .recv_timeout(DEADLINE)
        .expect("工作线程应到达排队边界");
    assert!(owner.cancel().expect("排队取消不得等待引擎锁"));
    let outcome = finished_rx.recv_timeout(DEADLINE);
    let did_initialize = initialized.load(Ordering::Acquire);
    // 先保存锁仍被占用时的结果，再释放以保证失败时工作线程也可收尾。
    drop(held_engine);
    worker.join().expect("工作线程应正常退出");

    assert!(outcome.expect("不放引擎锁也应及时收到取消"));
    assert!(!did_initialize, "取消排队请求不得开始后续初始化");
}

#[test]
fn cancelled_task_cannot_take_free_engine_lock_but_new_task_can() {
    let owner = CancelOwner::new();
    let cancelled = owner.token();
    let engine = Mutex::new(41);
    assert!(owner.cancel().expect("首次取消应成功"));
    assert_cancelled(wait_for_lock(&engine, &cancelled));
    assert!(engine.try_lock().is_some(), "取消结果不能遗留引擎锁");

    let next_owner = CancelOwner::new();
    let next = next_owner.token();
    let mut guard = wait_for_lock(&engine, &next).expect("新任务应正常获得引擎锁");
    *guard += 1;
    assert_eq!(*guard, 42);
    next.checkpoint().expect("旧取消不得影响新任务");
}

#[test]
fn concurrent_registration_cannot_lose_accepted_cancel() {
    let owner = Arc::new(CancelOwner::new());
    let task = owner.token();
    let run = Arc::new(CountingRun::default());
    let start = Arc::new(Barrier::new(3));
    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    let worker_run = run.clone();
    let registration_start = start.clone();
    let registration_worker = thread::spawn(move || {
        registration_start.wait();
        match task.register(worker_run) {
            Ok(registration) => {
                // 注册成功时一直保留 guard，直到首次取消调用确实返回。
                let completed = cancelled_rx.recv_timeout(DEADLINE).is_ok();
                drop(registration);
                (true, completed)
            }
            Err(error) => (false, is_cancelled(error.as_ref())),
        }
    });
    let cancellation_start = start.clone();
    let cancelling_owner = owner.clone();
    let cancellation_worker = thread::spawn(move || {
        cancellation_start.wait();
        let result = cancelling_owner.cancel().map_err(|error| error.to_string());
        let _ = cancelled_tx.send(());
        result
    });

    start.wait();
    let accepted = cancellation_worker.join().expect("取消线程应正常退出");
    let (registered, correct_boundary) = registration_worker.join().expect("注册线程应正常退出");

    assert!(accepted.expect("首次取消控制应成功"));
    assert!(correct_boundary, "注册须成功并收到终止，或以专门取消拒绝");
    assert_eq!(count(&run), usize::from(registered));
    assert_cancelled(owner.token().checkpoint());
}

#[test]
fn paused_old_termination_does_not_block_or_cancel_new_owner() {
    let old_owner = CancelOwner::new();
    let old_task = old_owner.token();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (released_tx, released_rx) = mpsc::channel();
    let old_run = Arc::new(BlockingRun {
        calls: AtomicUsize::new(0),
        entered: entered_tx,
        released: Mutex::new(released_rx),
    });
    let old_registration = old_task.register(old_run.clone()).expect("旧任务可注册");
    let cancellation_worker =
        thread::spawn(move || old_owner.cancel().map_err(|error| error.to_string()));
    entered_rx
        .recv_timeout(DEADLINE)
        .expect("旧终止应已进入回调");
    assert_cancelled(old_task.checkpoint());

    let new_owner = CancelOwner::new();
    let new_task = new_owner.token();
    let new_run = Arc::new(CountingRun::default());
    let new_registration = new_task
        .register(new_run.clone())
        .expect("新任务应独立可用");
    drop(old_registration);
    new_task.checkpoint().expect("旧任务解绑不取消新任务");
    assert_eq!(count(&new_run), 0);
    released_tx.send(()).expect("释放旧终止回调");
    assert!(cancellation_worker
        .join()
        .expect("旧取消线程应正常退出")
        .expect("旧终止控制应成功"));

    new_task.checkpoint().expect("旧回调完成不得改变新任务");
    assert_eq!(old_run.calls.load(Ordering::Acquire), 1);
    assert_eq!(count(&new_run), 0);
    assert!(new_owner.cancel().expect("新任务有自己的取消权"));
    assert_eq!(count(&new_run), 1);
    drop(new_registration);
}
