//! 显式运行的静音设备探针；只证明 WASAPI 队列和设备时钟，不证明解码或界面呈现。
use super::Output;
use crate::{
    media_foundation::MfRuntime, preview_audio_clock::ClockPhase,
    preview_audio_device_clock::PublishedClock, Result,
};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT;

/// 这里只换算已提交 PCM 帧的媒体长度；设备位置/QPC 的换算仍由正式时钟模型完成。
fn submit_silence(output: &mut Output, origin: Duration, rate: f64) -> Result<Duration> {
    let frames = (output.format.rate / 20).max(1).min(output.size);
    unsafe {
        let _buffer = output.writer.GetBuffer(frames)?;
        output
            .writer
            .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
    }
    // ReleaseBuffer 成功后才允许扩大队列的媒体终点。
    Ok(origin
        + Duration::from_secs_f64(f64::from(frames) / f64::from(output.format.rate)).mul_f64(rate))
}

fn start(output: &mut Output) -> Result<()> {
    unsafe { output.client.Start()? };
    output.started = true;
    Ok(())
}

fn wait_tick(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(!remaining.is_zero(), "静音设备时钟探针超过三秒截止时间");
    std::thread::sleep(remaining.min(Duration::from_millis(10)));
}

fn assert_observation(previous: PublishedClock, current: PublishedClock, queued_until: Duration) {
    assert_eq!(current.transport_generation, previous.transport_generation);
    assert_eq!(current.clock.generation, previous.clock.generation);
    assert!(current.clock.qpc_100ns >= previous.clock.qpc_100ns);
    assert!(current.clock.media_position >= previous.clock.media_position);
    assert!(current.clock.media_position <= queued_until);
    assert_eq!(
        current.clock.media_position + current.clock.queued_media,
        queued_until
    );
}

fn drain(
    output: &mut Output,
    baseline: PublishedClock,
    queued_until: Duration,
    deadline: Instant,
) -> Result<PublishedClock> {
    start(output)?;
    let mut previous = baseline;
    loop {
        assert!(Instant::now() < deadline, "静音设备队列未在三秒内排空");
        // 正式适配器每次使用 GetPosition 同次返回的设备位置与 100ns QPC。
        let current = output.clock.observe(queued_until, ClockPhase::Draining)?;
        assert_observation(previous, current, queued_until);
        assert!(matches!(
            current.clock.phase,
            ClockPhase::Draining | ClockPhase::Ended
        ));
        if current.clock.phase == ClockPhase::Ended {
            assert_eq!(current.clock.media_position, queued_until);
            assert!(current.clock.queued_media.is_zero());
            assert!(!current.clock.advancing);
            return Ok(current);
        }
        previous = current;
        wait_tick(deadline);
    }
}

fn reanchor(
    output: &mut Output,
    previous: PublishedClock,
    tag: u64,
    origin: Duration,
    rate: f64,
) -> Result<PublishedClock> {
    output.reset()?;
    output.clock.reanchor(tag, origin, rate, origin)?;
    let current = output.clock.observe(origin, ClockPhase::Priming)?;
    assert_eq!(current.transport_generation, tag);
    assert_ne!(current.transport_generation, previous.transport_generation);
    assert_eq!(
        current.clock.generation,
        previous.clock.generation.wrapping_add(1)
    );
    assert!(current.clock.qpc_100ns >= previous.clock.qpc_100ns);
    assert_eq!(current.clock.media_position, origin);
    assert!(current.clock.queued_media.is_zero());
    assert!(!current.clock.advancing);
    Ok(current)
}

#[test]
#[ignore = "显式连接默认音频设备，仅提交静音 PCM 并验证设备时钟"]
fn preview_audio_device_probe_silent_clock_pause_reset_and_rate() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    // COM/MF 先创建、最后释放；Output 的析构始终先 Stop/Reset 设备。
    let _runtime = MfRuntime::new()?;
    let mut output = Output::open()?;
    let initial = output.clock.observe(Duration::ZERO, ClockPhase::Priming)?;
    let queued_until = submit_silence(&mut output, Duration::ZERO, 1.)?;
    assert!(queued_until > Duration::ZERO);
    start(&mut output)?;
    let playing = output.clock.observe(queued_until, ClockPhase::Playing)?;
    assert_observation(initial, playing, queued_until);
    unsafe { output.client.Stop()? };
    output.started = false;
    let paused = output.clock.observe(queued_until, ClockPhase::Paused)?;
    assert_observation(playing, paused, queued_until);
    assert_eq!(paused.clock.phase, ClockPhase::Paused);
    assert!(!paused.clock.advancing);
    wait_tick(deadline);
    let still_paused = output.clock.observe(queued_until, ClockPhase::Paused)?;
    assert_observation(paused, still_paused, queued_until);
    assert_eq!(
        still_paused.clock.media_position,
        paused.clock.media_position
    );

    let origin = Duration::from_secs(1);
    let normal = reanchor(&mut output, still_paused, 1, origin, 1.)?;
    let normal_end = submit_silence(&mut output, origin, 1.)?;
    let normal_done = drain(&mut output, normal, normal_end, deadline)?;

    let double = reanchor(&mut output, normal_done, 2, normal_end, 2.)?;
    let double_end = submit_silence(&mut output, normal_end, 2.)?;
    assert_eq!(double_end - normal_end, (normal_end - origin).mul_f64(2.));
    let double_done = drain(&mut output, double, double_end, deadline)?;
    assert_eq!(double_done.clock.media_position, double_end);

    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "scope": "silent_wasapi_device_clock",
            "sample_rate": output.format.rate,
            "normal_media_us": (normal_end - origin).as_micros(),
            "double_media_us": (double_end - normal_end).as_micros(),
        })
    );
    Ok(())
}
