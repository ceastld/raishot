//! 帧租约的 Windows 后端；HRESULT 精确分类，不把提交或等待结束当作 GPU 完成。
use super::frame_lease::{Backend, QueryStatus};
use crate::Result;
use std::{
    error::Error,
    fmt,
    mem::size_of,
    time::{Duration, Instant},
};
use windows::{
    core::{Interface, HRESULT},
    Win32::Graphics::{
        Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D},
        Dxgi::IDXGIOutputDuplication,
    },
};

pub(super) struct DxgiFrame {
    duplication: Option<IDXGIOutputDuplication>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    query: ID3D11Query,
    started: Option<Instant>,
    // 固定两个 COM 引用；排空前保留源桌面纹理与目的自有纹理，不新增像素副本。
    _copy_resources: [Option<ID3D11Texture2D>; 2],
}

impl DxgiFrame {
    /// 仅在 AcquireNextFrame 成功后创建，所有引用属于这一帧的归还责任。
    pub(super) fn new(
        duplication: &IDXGIOutputDuplication,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        query: &ID3D11Query,
    ) -> Self {
        Self {
            duplication: Some(duplication.clone()),
            device: device.clone(),
            context: context.clone(),
            query: query.clone(),
            started: None,
            _copy_resources: [None, None],
        }
    }

    pub(super) fn retain_copy(&mut self, source: &ID3D11Texture2D, destination: &ID3D11Texture2D) {
        self._copy_resources = [Some(source.clone()), Some(destination.clone())];
    }
}

impl Backend for DxgiFrame {
    fn seal(&mut self) {
        self.started = Some(Instant::now());
        unsafe {
            self.context.End(&self.query);
            // Flush 只提交。是否完成仍由下面的 EVENT 查询判定。
            self.context.Flush();
        }
    }

    fn poll(&mut self) -> Result<QueryStatus> {
        let mut complete = windows::core::BOOL(0);
        // windows 0.61 的 GetData 包装器 .ok() 会把 S_FALSE 丢成 Ok(())。
        // 调用同一锁定接口的 vtable，保留精确 HRESULT；只在 S_OK 时消费 BOOL。
        let status = unsafe {
            (self.context.vtable().GetData)(
                self.context.as_raw(),
                self.query.as_raw(),
                (&mut complete as *mut windows::core::BOOL).cast(),
                size_of::<windows::core::BOOL>() as u32,
                0,
            )
        };
        query_status(status, complete.as_bool())
    }

    fn removed(&mut self) -> Result<()> {
        // S_OK 仅表示没有设备失败诊断，不能使 pending 变成 complete。
        unsafe { self.device.GetDeviceRemovedReason().map_err(Into::into) }
    }

    fn elapsed(&self) -> Duration {
        self.started
            .map_or(Duration::ZERO, |started| started.elapsed())
    }

    fn pause(&mut self, failing: bool) {
        std::thread::sleep(Duration::from_millis(if failing { 10 } else { 1 }));
    }

    fn release(&mut self) -> Result<()> {
        // 即使驱动返回错误也消费这次调用，Drop 不能再调用同一帧。
        let duplication = self.duplication.take().ok_or("帧归还责任已经消费")?;
        unsafe { duplication.ReleaseFrame().map_err(Into::into) }
    }
}

#[derive(Debug)]
struct UnexpectedQueryStatus(i32);

impl fmt::Display for UnexpectedQueryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GPU 完成查询返回未约定成功码 0x{:08X}", self.0 as u32)
    }
}

impl Error for UnexpectedQueryStatus {}

fn query_status(status: HRESULT, complete: bool) -> Result<QueryStatus> {
    match status.0 {
        0 if complete => Ok(QueryStatus::Complete), // S_OK + EVENT TRUE
        0 | 1 => Ok(QueryStatus::Pending),          // S_OK FALSE 或 S_FALSE（忽略未就绪输出）
        _ if status.is_err() => Err(windows::core::Error::from_hresult(status).into()),
        other => Err(UnexpectedQueryStatus(other).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_failure_only_s_ok_and_true_prove_event_completion() {
        assert_eq!(
            query_status(HRESULT(0), true).unwrap(),
            QueryStatus::Complete
        );
        assert_eq!(
            query_status(HRESULT(0), false).unwrap(),
            QueryStatus::Pending
        );
    }

    #[test]
    fn acquire_failure_s_false_never_consumes_stale_true_output() {
        for output in [false, true] {
            assert_eq!(
                query_status(HRESULT(1), output).unwrap(),
                QueryStatus::Pending
            );
        }
    }

    #[test]
    fn acquire_failure_device_hresult_remains_a_real_error() {
        for code in [0x887A_0005_u32, 0x887A_0006, 0x887A_0007] {
            let error = query_status(HRESULT(code as i32), true).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<windows::core::Error>()
                    .unwrap()
                    .code()
                    .0,
                code as i32
            );
            assert!(!error.is::<crate::capture::cancellation::CaptureCancelled>());
        }
    }

    #[test]
    fn acquire_failure_other_success_codes_cannot_approve_release() {
        for complete in [false, true] {
            let error = query_status(HRESULT(2), complete).unwrap_err();
            assert_eq!(error.downcast_ref::<UnexpectedQueryStatus>().unwrap().0, 2);
        }
    }
}
