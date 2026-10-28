//! 显式请求的有界摄像头录屏接点。只消费 owner 的 latest BGRA，不改屏幕录制时间轴。
//! 缺帧不等待驱动，不自动换设备或重启；关闭回执与画面降级提示分别保留。
use super::{
    frame::composite_camera, CameraCapture, CameraPhase, CameraRequest, CameraSample, CameraStatus,
};
use crate::{
    capture::Image, geometry::ScreenRect, recording_size_layout::RecordingSizeLayout, Result,
};
use std::{
    sync::TryLockError,
    time::{Duration, Instant},
};

const MAX_FRAME_AGE: Duration = Duration::from_millis(500);
const STOP_WAIT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub(crate) struct CameraRecordingRequest {
    pub camera: CameraRequest,
    pub destination: ScreenRect,
}
impl CameraRecordingRequest {
    pub(crate) fn validate(&self, region: ScreenRect, experimental: bool) -> Result<()> {
        if !experimental {
            return Err(tr!(
                "摄像头录屏仍在内部验证，请先启用媒体实验入口",
                "Camera recording is under internal validation; enable the media experiment first"
            )
            .into());
        }
        self.camera.validate()?;
        let encoded = RecordingSizeLayout::new(region)?.encoded_region();
        let destination = self.destination;
        if destination.width == 0 || destination.height == 0 {
            return Err(tr!(
                "摄像头画中画目标尺寸为空",
                "The camera destination is empty"
            )
            .into());
        }
        let right = i64::from(destination.left) + i64::from(destination.width);
        let bottom = i64::from(destination.top) + i64::from(destination.height);
        if right > i64::from(i32::MAX) || bottom > i64::from(i32::MAX) {
            return Err(tr!(
                "摄像头画中画超出物理坐标范围",
                "The camera destination exceeds the physical coordinate range"
            )
            .into());
        }
        let encoded_right = i64::from(encoded.left) + i64::from(encoded.width);
        let encoded_bottom = i64::from(encoded.top) + i64::from(encoded.height);
        if right <= i64::from(encoded.left)
            || bottom <= i64::from(encoded.top)
            || i64::from(destination.left) >= encoded_right
            || i64::from(destination.top) >= encoded_bottom
        {
            return Err(tr!(
                "摄像头画中画目标不在可编码的录屏区域内",
                "The camera destination does not intersect the encoded recording area"
            )
            .into());
        }
        // 部分越出 ROI 的目标仍按完整全局矩形 fit 后裁剪，不能裁后重新锚定。
        Ok(())
    }
}

pub(crate) struct CameraRecordingSession {
    capture: CameraCapture,
    region: ScreenRect,
    destination: ScreenRect,
    delivery: Delivery,
}
impl CameraRecordingSession {
    pub(crate) fn start(
        request: CameraRecordingRequest,
        region: ScreenRect,
        experimental: bool,
    ) -> Result<Self> {
        request.validate(region, experimental)?;
        let capture = CameraCapture::start(request.camera)?;
        Ok(Self {
            capture,
            region,
            destination: request.destination,
            delivery: Delivery::default(),
        })
    }

    pub(crate) fn set_paused(&mut self, paused: bool, now: Instant) {
        if self.delivery.paused == paused {
            return;
        }
        self.delivery.set_paused(paused, now);
        // 邮箱忙时也不阻塞暂停边界；恢复时点会过滤尚未来得及清掉的旧帧。
        match self.capture.shared.inbox.try_lock() {
            Ok(mut inbox) => inbox.latest = None,
            Err(TryLockError::Poisoned(error)) => error.into_inner().latest = None,
            Err(TryLockError::WouldBlock) => {}
        }
    }

    /// now 是调用方已有时点；取出邮箱后补采当前时点，避免较早的屏幕帧时间误拒新到帧。
    pub(crate) fn composite(
        &mut self,
        image: &mut Image,
        region: ScreenRect,
        now: Instant,
    ) -> Option<String> {
        if region != self.region {
            self.delivery.terminal = true;
            self.delivery.cached = None;
            return self.delivery.warn_once(
                tr!(
                    "摄像头合成区域与本次录屏区域不一致",
                    "The camera composition area changed during this recording"
                )
                .into(),
            );
        }
        // 作为 camera child 可以原子地拿到状态和 latest；锁忙时绝不等待 callback/owner。
        let snapshot = match self.capture.shared.inbox.try_lock() {
            Ok(mut inbox) => Some((inbox.status.clone(), inbox.latest.take())),
            Err(TryLockError::Poisoned(error)) => {
                let mut inbox = error.into_inner();
                Some((inbox.status.clone(), inbox.latest.take()))
            }
            Err(TryLockError::WouldBlock) => None,
        };
        let observed_now = now.max(Instant::now());
        let warning = match snapshot {
            Some((status, sample)) => self.delivery.update(Some(&status), sample, observed_now),
            None => self.delivery.update(None, None, observed_now),
        };
        if self.delivery.paused || self.delivery.terminal {
            return warning;
        }
        let Some(sample) = &self.delivery.cached else {
            return warning;
        };
        if let Err(error) = composite_visible(image, region, &sample.frame, self.destination) {
            self.delivery.cached = None;
            self.delivery.terminal = true;
            return warning.or_else(|| self.delivery.warn_once(error.to_string()));
        }
        warning
    }

    pub(crate) fn finish(self) -> (CameraStatus, Option<String>) {
        let status = self.capture.stop_and_wait(STOP_WAIT);
        let warning = cleanup_warning(&status);
        (status, warning)
    }
    // 未调用 finish 的提前退出仍由 CameraCapture::drop 关闭交付；不在析构中等待驱动。
}

/// 只含普通像素与时点，普通测试不需要构造 CameraCapture 或访问任何设备。
#[derive(Default)]
struct Delivery {
    cached: Option<CameraSample>,
    paused: bool,
    resume_floor: Option<Instant>,
    terminal: bool,
    warned: bool,
}
impl Delivery {
    fn set_paused(&mut self, paused: bool, now: Instant) {
        self.cached = None;
        self.paused = paused;
        if !paused {
            self.resume_floor = Some(now);
        }
    }

    fn update(
        &mut self,
        status: Option<&CameraStatus>,
        sample: Option<CameraSample>,
        now: Instant,
    ) -> Option<String> {
        if let Some(reason) = status.and_then(terminal_reason) {
            self.cached = None;
            self.terminal = true;
            return self.warn_once(reason);
        }
        if self.paused || self.terminal {
            self.cached = None;
            return None;
        }
        if let Some(sample) = sample {
            // 暂停区间及 resume 之前的已排队帧是正常过滤，不作为时钟/设备故障提示。
            if self
                .resume_floor
                .is_none_or(|floor| sample.received >= floor)
            {
                self.cached = Some(sample);
            }
        }
        let Some(sample) = &self.cached else {
            return None;
        };
        // Windows 的极小逆序差可能被差值计算归零，严格未来门必须先比较时点。
        let age = if sample.received > now {
            None
        } else {
            now.checked_duration_since(sample.received)
        };
        let reason = match age {
            None => Some(tr!(
                "摄像头帧的接收时点位于未来，已丢弃",
                "A camera frame had a future receipt time and was discarded"
            )),
            Some(age) if age > MAX_FRAME_AGE => Some(tr!(
                "摄像头画面已超过 500 毫秒未更新",
                "The camera image has not updated for more than 500 ms"
            )),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            self.cached = None;
            return self.warn_once(reason.into());
        }
        None
    }

    fn warn_once(&mut self, reason: String) -> Option<String> {
        if self.warned {
            return None;
        }
        self.warned = true;
        Some(trf!(
            "{reason}；本次屏幕录制继续，摄像头画面不显示",
            "{reason}; screen recording continues without the camera image"
        ))
    }
}

fn terminal_reason(status: &CameraStatus) -> Option<String> {
    if let Some(issue) = &status.failure {
        let issue = short_issue(issue);
        return Some(trf!(
            "摄像头采集已停止：{issue}",
            "Camera capture stopped: {issue}"
        ));
    }
    if let Some(issue) = status.cleanup_issues.first() {
        let issue = short_issue(issue);
        return Some(trf!(
            "摄像头清理有诊断：{issue}",
            "Camera cleanup reported: {issue}"
        ));
    }
    if status.phase == CameraPhase::Failed {
        return Some(
            tr!(
                "摄像头采集已异常停止",
                "Camera capture stopped with an error"
            )
            .into(),
        );
    }
    if status.delivery_closed
        || status.worker_finished
        || matches!(status.phase, CameraPhase::Closing | CameraPhase::Closed)
    {
        return Some(tr!("摄像头画面交付已结束", "Camera image delivery ended").into());
    }
    None
}

fn cleanup_warning(status: &CameraStatus) -> Option<String> {
    if status.worker_finished
        && status.delivery_closed
        && status.phase == CameraPhase::Closed
        && status.failure.is_none()
        && status.cleanup_issues.is_empty()
    {
        return None;
    }
    let reason = status
        .failure
        .as_ref()
        .or_else(|| status.cleanup_issues.first())
        .map(short_issue)
        .unwrap_or_else(|| {
            tr!(
                "等待 250 毫秒后未收到无错误的完整关闭回执",
                "No clean completed shutdown was confirmed within the 250 ms wait"
            )
            .into()
        });
    Some(trf!(
        "摄像头停止仍有诊断：{reason}；屏幕录制收尾继续，不能据此确认设备已释放",
        "Camera shutdown reported: {reason}; screen recording finalization continues, and device release is not confirmed"
    ))
}

fn short_issue(issue: &super::CameraIssue) -> String {
    issue.to_string().chars().take(512).collect()
}

fn composite_visible(
    image: &mut Image,
    region: ScreenRect,
    frame: &super::frame::CameraFrame,
    destination: ScreenRect,
) -> Result<()> {
    let layout = RecordingSizeLayout::new(region)?;
    let visible = composite_camera(image, region, frame, destination)?;
    if visible.is_none_or(|rect| rect.x >= layout.encoded_width || rect.y >= layout.encoded_height)
    {
        return Err(tr!(
            "摄像头按比例缩放后的画面不在可编码区域内",
            "The fitted camera image does not intersect the encoded area"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{devices::CameraDeviceId, frame::CameraFrame};
    use super::*;

    fn request(destination: ScreenRect) -> CameraRecordingRequest {
        CameraRecordingRequest {
            camera: CameraRequest::new(CameraDeviceId::from_utf16(vec![65, 66]).unwrap()),
            destination,
        }
    }

    fn frame(value: u8) -> CameraFrame {
        CameraFrame {
            width: 2,
            height: 2,
            bgra: [value, 0, 0, 255].repeat(4),
        }
    }

    fn sample(received: Instant, sequence: u64) -> CameraSample {
        CameraSample {
            frame: frame(sequence as u8),
            source_timestamp_100ns: -12,
            received,
            sequence,
        }
    }

    fn ready() -> CameraStatus {
        CameraStatus {
            phase: CameraPhase::Ready,
            ..Default::default()
        }
    }

    #[test]
    fn recording_camera_session_preflight_checks_gate_bounds_and_encoded_intersection() {
        let region = ScreenRect {
            left: -20,
            top: 10,
            width: 5,
            height: 5,
        };
        let mut value = request(region);
        assert!(value.validate(region, true).is_ok());
        assert!(value.validate(region, false).is_err());
        value.destination = ScreenRect {
            left: -21,
            top: 9,
            width: 2,
            height: 2,
        };
        assert!(value.validate(region, true).is_ok());
        value.destination = ScreenRect {
            left: -16,
            top: 10,
            width: 1,
            height: 4,
        };
        assert!(value.validate(region, true).is_err(), "只在裁掉的右列里");
        value.destination = ScreenRect {
            left: -20,
            top: 14,
            width: 4,
            height: 1,
        };
        assert!(value.validate(region, true).is_err(), "只在裁掉的底行里");
        value.destination = ScreenRect { width: 0, ..region };
        assert!(value.validate(region, true).is_err());
        value.destination = ScreenRect {
            left: i32::MAX,
            width: 2,
            ..region
        };
        assert!(value.validate(region, true).is_err());
        value.destination = region;
        value.camera.run_budget = Duration::from_secs(61);
        assert!(
            value.validate(region, true).is_err(),
            "不能扩大 owner 的活动上限"
        );
    }

    #[test]
    fn recording_camera_session_cache_expires_and_rejects_future_receipts() {
        let start = Instant::now();
        let mut delivery = Delivery::default();
        assert!(delivery
            .update(Some(&ready()), Some(sample(start, 1)), start)
            .is_none());
        let fresh_limit = start + MAX_FRAME_AGE;
        assert!(delivery.update(None, None, fresh_limit).is_none());
        assert_eq!(delivery.cached.as_ref().unwrap().sequence, 1);
        let expired_now = fresh_limit + Duration::from_millis(1);
        assert!(
            expired_now > fresh_limit
                && expired_now
                    .checked_duration_since(start)
                    .is_some_and(|age| age > MAX_FRAME_AGE),
            "过期夹具必须超过 500ms：start={start:?}, fresh_limit={fresh_limit:?}, expired_now={expired_now:?}"
        );
        assert!(delivery.update(None, None, expired_now).is_some());
        assert!(delivery.cached.is_none());
        assert!(delivery
            .update(None, None, start + Duration::from_secs(1))
            .is_none());
        for offset in [Duration::from_nanos(1), Duration::from_millis(1)] {
            let mut future = Delivery::default();
            let future_received = start + offset;
            let inverse_age = start.checked_duration_since(future_received);
            assert!(
                future_received > start,
                "未来夹具必须晚于 now：start={start:?}, future_received={future_received:?}, inverse_age={inverse_age:?}"
            );
            assert!(
                future
                    .update(Some(&ready()), Some(sample(future_received, 2)), start)
                    .is_some(),
                "严格未来帧必须拒收，即使差值归零：offset={offset:?}, inverse_age={inverse_age:?}"
            );
            assert!(future.cached.is_none());
        }
        // 仍是同一 owner 的后续真实新帧，不是自动重启或切设备。
        assert!(delivery
            .update(
                Some(&ready()),
                Some(sample(start + Duration::from_secs(1), 3)),
                start + Duration::from_secs(1)
            )
            .is_none());
        assert_eq!(delivery.cached.as_ref().unwrap().sequence, 3);
    }

    #[test]
    fn recording_camera_session_pause_and_resume_filter_old_mailbox_samples() {
        let start = Instant::now();
        let mut delivery = Delivery::default();
        delivery.update(Some(&ready()), Some(sample(start, 1)), start);
        delivery.set_paused(true, start + Duration::from_millis(1));
        assert!(delivery.cached.is_none());
        assert!(delivery
            .update(
                Some(&ready()),
                Some(sample(start + Duration::from_millis(2), 2)),
                start + Duration::from_millis(2)
            )
            .is_none());
        assert!(delivery.cached.is_none());
        let resumed = start + Duration::from_secs(1);
        delivery.set_paused(false, resumed);
        for offset in [Duration::from_nanos(1), Duration::from_millis(1)] {
            let before_resume = resumed - offset;
            assert!(
                before_resume < resumed,
                "旧邮箱夹具必须早于恢复边界：before_resume={before_resume:?}, resumed={resumed:?}"
            );
            assert!(delivery
                .update(Some(&ready()), Some(sample(before_resume, 3)), resumed)
                .is_none());
            assert!(delivery.cached.is_none());
            assert!(!delivery.warned, "暂停过滤不是设备/坏时钟告警");
        }
        delivery.update(Some(&ready()), Some(sample(resumed, 4)), resumed);
        assert_eq!(delivery.cached.as_ref().unwrap().sequence, 4);
    }

    #[test]
    fn recording_camera_session_terminal_state_clears_cache_and_warns_once() {
        let start = Instant::now();
        for phase in [CameraPhase::Closed, CameraPhase::Failed] {
            let mut delivery = Delivery::default();
            delivery.update(Some(&ready()), Some(sample(start, 1)), start);
            let status = CameraStatus {
                phase,
                delivery_closed: true,
                worker_finished: true,
                ..Default::default()
            };
            assert!(delivery
                .update(Some(&status), Some(sample(start, 2)), start)
                .is_some());
            assert!(delivery.cached.is_none());
            assert!(delivery.update(Some(&status), None, start).is_none());
            delivery.update(Some(&ready()), Some(sample(start, 3)), start);
            assert!(delivery.cached.is_none(), "终态不能被旧/晚帧复活");
        }
        let closing = CameraStatus {
            phase: CameraPhase::Closing,
            delivery_closed: true,
            ..Default::default()
        };
        assert!(cleanup_warning(&closing).is_some());
        let closed = CameraStatus {
            phase: CameraPhase::Closed,
            delivery_closed: true,
            worker_finished: true,
            ..Default::default()
        };
        assert!(cleanup_warning(&closed).is_none());
        let failed = CameraStatus {
            phase: CameraPhase::Failed,
            ..closed
        };
        assert!(
            cleanup_warning(&failed).is_some(),
            "完成线程不等于无错误关闭"
        );
    }

    #[test]
    fn recording_camera_session_actual_fit_must_reach_encoded_pixels() {
        let region = ScreenRect {
            left: -10,
            top: 2,
            width: 5,
            height: 5,
        };
        let destination = ScreenRect {
            left: -7,
            top: 2,
            width: 3,
            height: 4,
        };
        assert!(request(destination).validate(region, true).is_ok());
        let mut image = Image {
            width: 5,
            height: 5,
            bgra: [9, 8, 7, 255].repeat(25),
            readback_ms: 0.,
        };
        let narrow = CameraFrame {
            width: 1,
            height: 4,
            bgra: [70, 80, 90, 255].repeat(4),
        };
        assert!(
            composite_visible(&mut image, region, &narrow, destination).is_err(),
            "destination 有交集，但居中窄 fit 只落在 x=4 的裁掉列"
        );
        for row in image.bgra.chunks_exact(5 * 4) {
            assert_eq!(&row[..4 * 4], &[9, 8, 7, 255].repeat(4));
        }
        let visible = ScreenRect {
            left: -9,
            top: 3,
            width: 2,
            height: 2,
        };
        composite_visible(&mut image, region, &frame(50), visible).unwrap();
        assert_eq!(&image.bgra[(5 + 1) * 4..(5 + 1) * 4 + 4], &[50, 0, 0, 255]);
    }
}
