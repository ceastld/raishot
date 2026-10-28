//! 对合成文件运行真实 Player 控制队列；没有 GPUI 窗口，也不创建音频输出。
use crate::{audio_capture::MixLevels, recording_preview::Player, Result};
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn wait_ready(player: &Player, frame: bool) -> Result<Option<(u64, Duration, Vec<u8>)>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(error) = player.error() {
            return Err(error.into());
        }
        if !player.seeking() {
            let taken = if frame {
                player.take_frame_with_time()
            } else {
                None
            };
            if !frame || taken.is_some() {
                return Ok(taken);
            }
        }
        if Instant::now() >= deadline {
            return Err("播放器控制自检未在三秒内完成".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn run(path: &Path) -> Result<Value> {
    let timeline: Vec<_> = (0..super::FRAMES).map(super::video_time).collect();
    run_timeline(path, &timeline, Duration::from_millis(1520))
}

pub(super) fn run_timeline(path: &Path, timeline: &[Duration], target: Duration) -> Result<Value> {
    // 空分轨显式禁止声音设备。这里验证真实解码和控制，不验证内嵌声音输出。
    let mut player = Player::open(path.to_path_buf(), &[], MixLevels::default());
    player.set_playing(false);
    let (_, first, _) = wait_ready(&player, true)?.unwrap();
    super::ensure(!player.playing(), "首帧加载期间请求暂停未生效")?;
    let duration = player.position().1;
    super::ensure(!duration.is_zero(), "播放器未发布媒体时长")?;

    let target = target.min(duration);
    player.seek_ratio((target.as_secs_f64() / duration.as_secs_f64()) as f32);
    player.set_rate(2.);
    let (serial, floor, _) = wait_ready(&player, true)?.unwrap();
    let floor_index = timeline
        .iter()
        .rposition(|time| *time <= target)
        .ok_or("测试目标之前没有视频帧")?;
    let expected_floor = timeline[floor_index];
    super::ensure(
        floor.abs_diff(expected_floor) <= Duration::from_millis(1),
        "精确跳转未保留目标所在帧",
    )?;
    super::ensure(
        player.position().0.abs_diff(target) <= Duration::from_millis(1),
        "跳转进度未锚在请求目标",
    )?;
    super::ensure(!player.playing(), "暂停跳转被变速意外恢复播放")?;

    player.step_forward();
    let (step_serial, step, _) = wait_ready(&player, true)?.unwrap();
    super::ensure(step_serial > serial && step > floor, "逐帧未推进真实解码帧")?;
    super::ensure(
        timeline
            .get(floor_index + 1)
            .is_some_and(|expected| step.abs_diff(*expected) <= Duration::from_millis(1)),
        "逐帧跨过了下一解码帧",
    )?;
    super::ensure(!player.playing(), "逐帧后未保持暂停")?;

    for ratio in [0.2, 0.8, 0.4] {
        player.seek_ratio(ratio);
    }
    let (_, latest, _) = wait_ready(&player, true)?.unwrap();
    let last_target = duration.mul_f32(0.4);
    let expected_latest = timeline
        .iter()
        .rfind(|time| **time <= last_target)
        .ok_or("最终测试目标之前没有视频帧")?;
    super::ensure(
        latest.abs_diff(*expected_latest) <= Duration::from_millis(1),
        "连续跳转未保留最终目标",
    )?;

    player.seek_ratio(1.);
    let (_, last, _) = wait_ready(&player, true)?.unwrap();
    let end_position = player.position().0;
    player.step_forward();
    wait_ready(&player, false)?;
    super::ensure(
        player.take_frame_with_time().is_none(),
        "片尾逐帧发布了额外画面",
    )?;
    super::ensure(
        player.position().0 == end_position && !player.playing(),
        "片尾逐帧改变了暂停位置",
    )?;
    Ok(
        json!({"ok":true,"scope":"player_control_without_audio_output", "first_seconds":first.as_secs_f64(),
        "seek_floor_seconds":floor.as_secs_f64(),"step_seconds":step.as_secs_f64(),
        "coalesced_seek_seconds":latest.as_secs_f64(),"last_seconds":last.as_secs_f64()}),
    )
}
