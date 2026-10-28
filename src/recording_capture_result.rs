//! 录屏采集的显式取消合同；失败不受后续停止请求影响。
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) struct CaptureCancelled;

impl fmt::Display for CaptureCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("采集已取消")
    }
}

impl Error for CaptureCancelled {}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CaptureOutcome<T> {
    Ready(T),
    Cancelled,
}

impl<T> CaptureOutcome<T> {
    /// 仅取消生成点发出的专用类型可以中断一帧；其它错误保持原对象。
    pub(crate) fn from_result(result: crate::Result<T>) -> crate::Result<Self> {
        match result {
            Ok(value) => Ok(Self::Ready(value)),
            Err(error) if error.is::<CaptureCancelled>() => Ok(Self::Cancelled),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct Failure(u64);

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "采集失败 {}", self.0)
        }
    }

    impl Error for Failure {}

    #[test]
    fn ready_transfers_value_without_copying() {
        let pixels = vec![1_u8, 2, 3, 4];
        let original = pixels.as_ptr();
        let CaptureOutcome::Ready(pixels) = CaptureOutcome::from_result(Ok(pixels)).unwrap() else {
            panic!("成功采集不能成为取消");
        };
        assert_eq!(pixels, [1, 2, 3, 4]);
        assert_eq!(pixels.as_ptr(), original);
    }

    #[test]
    fn explicit_cancellation_is_a_cancelled_outcome() {
        let outcome = CaptureOutcome::<()>::from_result(Err(CaptureCancelled.into())).unwrap();
        assert_eq!(outcome, CaptureOutcome::Cancelled);
        assert_eq!(CaptureCancelled.to_string(), "采集已取消");
    }

    #[test]
    fn matching_error_text_does_not_become_cancellation() {
        let failure = CaptureOutcome::<()>::from_result(Err("采集已取消".into())).unwrap_err();
        assert_eq!(failure.to_string(), CaptureCancelled.to_string());
        assert!(!failure.is::<CaptureCancelled>());
    }

    #[test]
    fn failure_keeps_the_original_error_object() {
        let failure: Box<dyn Error> = Box::new(Failure(73));
        let original = std::ptr::from_ref(failure.as_ref());
        let returned = CaptureOutcome::<()>::from_result(Err(failure)).unwrap_err();
        assert!(std::ptr::eq(
            original,
            std::ptr::from_ref(returned.as_ref())
        ));
        assert_eq!(returned.downcast_ref::<Failure>().unwrap().0, 73);
    }

    #[test]
    fn a_later_stop_request_does_not_reclassify_failure() {
        let stop = AtomicBool::new(false);
        let result: crate::Result<()> = Err(Failure(91).into());
        stop.store(true, Ordering::Release);
        let returned = CaptureOutcome::from_result(result).unwrap_err();
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(returned.downcast_ref::<Failure>().unwrap().0, 91);
    }
}
