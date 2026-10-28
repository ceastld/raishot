//! 在当前笔画时间范围内补回 Windows 合并的移动样本；最多 64 点，不插值猜测轨迹。
use crate::annotation::Point;
use windows::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::ClientToScreen,
    UI::{
        Input::KeyboardAndMouse::{GetMouseMovePointsEx, GMMP_USE_DISPLAY_POINTS, MOUSEMOVEPOINT},
        WindowsAndMessaging::GetMessageTime,
    },
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    pub x: i32,
    pub y: i32,
    pub time: u32,
}
impl Sample {
    fn position(self) -> (i32, i32) {
        (self.x, self.y)
    }
}
#[derive(Default)]
pub(crate) struct PencilSamples {
    last: Option<Sample>,
}
fn screen_sample(hwnd: HWND, client: Point) -> Option<(Sample, POINT)> {
    let mut origin = POINT::default();
    unsafe {
        ClientToScreen(hwnd, &mut origin).ok().ok()?;
    }
    let (x, y) = client.rounded();
    Some((
        Sample {
            x: x + origin.x,
            y: y + origin.y,
            time: unsafe { GetMessageTime() as u32 },
        },
        origin,
    ))
}
impl PencilSamples {
    pub fn clear(&mut self) {
        self.last = None;
    }
    pub fn begin(&mut self, hwnd: HWND, client: Point) {
        self.last = screen_sample(hwnd, client).map(|s| s.0);
    }
    pub fn collect(&mut self, hwnd: HWND, client: Point) -> Vec<Point> {
        let client = Point::from_tuple(client.rounded());
        let Some((current, origin)) = screen_sample(hwnd, client) else {
            self.clear();
            return vec![client];
        };
        let previous = self.last.replace(current);
        let Some(previous) = previous else {
            return vec![client];
        };
        // API 以 16 位显示坐标查找；负屏幕原点需要按低 16 位传入，再还原有符号值。
        let query = MOUSEMOVEPOINT {
            x: current.x & 0xffff,
            y: current.y & 0xffff,
            time: current.time,
            dwExtraInfo: 0,
        };
        let mut buffer = [MOUSEMOVEPOINT::default(); 64];
        let count = unsafe {
            GetMouseMovePointsEx(
                std::mem::size_of::<MOUSEMOVEPOINT>() as u32,
                &query,
                &mut buffer,
                GMMP_USE_DISPLAY_POINTS,
            )
        };
        if count <= 0 {
            return vec![client];
        }
        let samples: Vec<_> = buffer[..count as usize]
            .iter()
            .map(|p| Sample {
                x: p.x as u16 as i16 as i32,
                y: p.y as u16 as i16 as i32,
                time: p.time,
            })
            .collect();
        between(previous, current, &samples)
            .into_iter()
            .map(|p| Point {
                x: (p.x - origin.x) as f32,
                y: (p.y - origin.y) as f32,
            })
            .collect()
    }
}
// recent 按 Windows 的新到旧顺序。时间戳与前一事件共同限定笔画，绝不拼入落笔前的轨迹。
pub(crate) fn between(previous: Sample, current: Sample, recent: &[Sample]) -> Vec<Sample> {
    let duration = current.time.wrapping_sub(previous.time);
    if duration > i32::MAX as u32 {
        return vec![current];
    }
    let Some(head) = recent.iter().position(|p| {
        p.position() == current.position() && current.time.wrapping_sub(p.time) <= duration
    }) else {
        return vec![current];
    };
    let tail = recent
        .iter()
        .enumerate()
        .skip(head + 1)
        .find_map(|(i, p)| (*p == previous).then_some(i));
    let mut result = Vec::new();
    let mut last_age = duration;
    for &p in recent[head..tail.unwrap_or(recent.len())].iter().rev() {
        let age = current.time.wrapping_sub(p.time);
        if age > duration || (age == duration && tail.is_none()) {
            continue;
        }
        if age > last_age {
            return vec![current];
        }
        last_age = age;
        if result
            .last()
            .map_or(previous.position(), |p: &Sample| p.position())
            != p.position()
        {
            result.push(p);
        }
    }
    if result
        .last()
        .is_none_or(|p| p.position() != current.position())
    {
        result.push(current);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    fn s(x: i32, y: i32, time: u32) -> Sample {
        Sample { x, y, time }
    }
    #[test]
    fn coalesced_circle_batches_recover_the_same_path_as_full_sampling() {
        let raw: Vec<_> = (0..=400)
            .map(|i| {
                let t = i as f32 / 400. * std::f32::consts::TAU * 2.;
                s(
                    (-900. + 23. * t.cos()).round() as i32,
                    (-200. + 50. * t.sin()).round() as i32,
                    100 + i,
                )
            })
            .collect();
        let mut full = crate::annotation::Annotation::new(
            crate::annotation::Tool::Pen,
            Point::from_tuple(raw[0].position()),
            Default::default(),
        );
        let mut recovered = full.clone();
        for p in &raw[1..] {
            full.update(Point::from_tuple(p.position()));
        }
        let mut previous = raw[0];
        for end in (17..raw.len())
            .step_by(17)
            .chain(std::iter::once(raw.len() - 1))
        {
            let recent: Vec<_> = raw[end.saturating_sub(63)..=end]
                .iter()
                .copied()
                .rev()
                .collect();
            for p in between(previous, raw[end], &recent) {
                recovered.update(Point::from_tuple(p.position()));
            }
            previous = raw[end];
        }
        assert_eq!(
            recovered.points, full.points,
            "batched input lost or reordered samples"
        );
        assert_eq!(recovered.vector_path(), full.vector_path());
    }
    #[test]
    fn pencil_samples_recovers_order_without_joining_previous_stroke_or_future() {
        let before = s(-200, -100, 10);
        let end = s(-170, -90, 40);
        let recent = [
            s(999, 999, 41),
            end,
            s(-180, -80, 30),
            s(-190, -95, 20),
            before,
            s(800, 700, 9),
        ];
        assert_eq!(
            between(before, end, &recent),
            vec![recent[3], recent[2], end]
        );
        assert_eq!(between(before, s(77, 88, 40), &recent), vec![s(77, 88, 40)]);
    }
    #[test]
    fn pencil_samples_keeps_same_timestamp_points_once_and_handles_clock_wrap() {
        let before = s(0, 0, u32::MAX - 2);
        let a = s(1, 2, 0);
        let b = s(3, 4, 0);
        let end = s(5, 6, 2);
        assert_eq!(between(before, end, &[end, b, a, before]), vec![a, b, end]);
        assert_eq!(between(a, end, &[end, b, a, before]), vec![b, end]);
        assert_eq!(between(b, end, &[end, b, a, before]), vec![end]);
    }
    #[test]
    fn pencil_samples_bounds_missing_history_and_rejects_reordered_records() {
        let before = s(0, 0, 10);
        let end = s(9, 9, 20);
        assert_eq!(
            between(before, end, &[end, s(2, 2, 14), s(1, 1, 10), s(8, 8, 9)]),
            vec![s(2, 2, 14), end]
        );
        assert_eq!(
            between(before, end, &[end, s(1, 1, 12), s(2, 2, 14)]),
            vec![end]
        );
        assert_eq!(between(before, end, &[]), vec![end]);
    }
}
