use super::*;

#[test]
fn audio_capture_drop_broadcasts_stop_before_joining_sources() {
    let first_stop = Arc::new(AtomicBool::new(false));
    let second_stop = Arc::new(AtomicBool::new(false));
    let saw_broadcast_before_exit = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        buffer: Mutex::new(CaptureBuffer::new(1).unwrap()),
        clock: Mutex::new(CaptureClockObserver::new()),
        monitor: Mutex::new(SourceMonitor::new(EndpointSelection::Default)),
        worker_exit: WorkerExit::completed(),
        error: Mutex::new(None),
        warning: Mutex::new(None),
        ready: AtomicBool::new(false),
    });
    let mut capture = AudioCapture {
        sources: vec![
            SourceThread {
                source: AudioSource::System,
                stop: first_stop.clone(),
                shared: shared.clone(),
                thread: None,
            },
            SourceThread {
                source: AudioSource::Microphone,
                stop: second_stop.clone(),
                shared,
                thread: None,
            },
        ],
    };
    let observed = saw_broadcast_before_exit.clone();
    capture.sources[0].thread = Some(std::thread::spawn(move || {
        while !first_stop.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // 旧的逐来源stop/join会等到此期限；先广播的析构无需等待期限。
        let deadline = Instant::now() + Duration::from_secs(2);
        while !second_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        observed.store(second_stop.load(Ordering::Acquire), Ordering::Release);
    }));
    drop(capture);
    assert!(
        saw_broadcast_before_exit.load(Ordering::Acquire),
        "the first joined source must observe the other source's stop request"
    );
}

#[test]
fn audio_capture_stop_and_wait_retains_terminal_errors_and_is_repeatable() {
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        buffer: Mutex::new(CaptureBuffer::new(1).unwrap()),
        clock: Mutex::new(CaptureClockObserver::new()),
        monitor: Mutex::new(SourceMonitor::new(EndpointSelection::Default)),
        worker_exit: WorkerExit::pending(),
        error: Mutex::new(None),
        warning: Mutex::new(None),
        ready: AtomicBool::new(true),
    });
    let worker_stop = stop.clone();
    let worker_shared = shared.clone();
    let thread = std::thread::spawn(move || {
        let _exited = worker_shared.worker_exit.guard();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        if worker_stop.load(Ordering::Acquire) {
            *worker_shared.error.lock().unwrap() = Some("terminal stop failure".into());
        }
    });
    let mut capture = AudioCapture {
        sources: vec![SourceThread {
            source: AudioSource::System,
            stop: stop.clone(),
            shared: shared.clone(),
            thread: Some(thread),
        }],
    };

    capture.stop_and_wait();
    assert!(stop.load(Ordering::Acquire));
    assert!(shared.worker_exit.is_exited());
    assert!(capture.sources[0].thread.is_none());
    let errors = capture.errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("terminal stop failure"));

    capture.stop_and_wait();
    assert!(capture.sources[0].thread.is_none());
    assert_eq!(capture.errors(), errors);
}
