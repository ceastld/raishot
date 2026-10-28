//! 验收脚本共用的本进程内存读数：CPU 工作集／提交量与 DXGI 适配器 node 0 的显存用量。
use crate::Result;
use std::mem::size_of;
use windows::{
    core::Interface,
    Win32::{
        Graphics::Dxgi::*,
        System::{ProcessStatus::*, Threading::GetCurrentProcess},
    },
};

pub(crate) fn cpu_memory() -> Result<serde_json::Value> {
    let mut value = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut value as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            value.cb,
        )
        .ok()?;
    }
    let mib = |bytes: usize| bytes as f64 / 1_048_576.;
    Ok(serde_json::json!({
        "working_set_bytes":value.WorkingSetSize,"private_bytes":value.PrivateUsage,
        "peak_working_set_bytes":value.PeakWorkingSetSize,"peak_commit_bytes":value.PeakPagefileUsage,
        "working_set_mib":mib(value.WorkingSetSize),"private_mib":mib(value.PrivateUsage),
        "peak_working_set_mib":mib(value.PeakWorkingSetSize),
        "peak_commit_mib":mib(value.PeakPagefileUsage),
        "peak_commit_is_not_peak_private":true
    }))
}
pub(crate) fn gpu_memory() -> serde_json::Value {
    let result = (|| -> windows::core::Result<serde_json::Value> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut adapters = Vec::new();
        let mut local_usage = 0_u64;
        let mut non_local_usage = 0_u64;
        let mut local_reservation = 0_u64;
        let mut non_local_reservation = 0_u64;
        let mut complete = true;
        let mut measured = 0;
        for index in 0..32 {
            let adapter = match unsafe { factory.EnumAdapters1(index) } {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error),
            };
            let name = unsafe { adapter.GetDesc1() }
                .ok()
                .map(|desc| {
                    let n = desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len());
                    String::from_utf16_lossy(&desc.Description[..n])
                })
                .unwrap_or_else(|| format!("adapter-{index}"));
            let query = (|| -> windows::core::Result<_> {
                let adapter: IDXGIAdapter3 = adapter.cast()?;
                let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                let mut non_local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                unsafe {
                    adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local)?;
                    adapter.QueryVideoMemoryInfo(
                        0,
                        DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
                        &mut non_local,
                    )?;
                }
                Ok((local, non_local))
            })();
            match query {
                Ok((local, non_local)) => {
                    measured += 1;
                    local_usage = local_usage.saturating_add(local.CurrentUsage);
                    non_local_usage = non_local_usage.saturating_add(non_local.CurrentUsage);
                    local_reservation = local_reservation.saturating_add(local.CurrentReservation);
                    non_local_reservation =
                        non_local_reservation.saturating_add(non_local.CurrentReservation);
                    adapters.push(serde_json::json!({"index":index,"name":name,"available":true,
                        "local":{"usage_bytes":local.CurrentUsage,"reservation_bytes":local.CurrentReservation,
                            "budget_bytes":local.Budget},
                        "non_local":{"usage_bytes":non_local.CurrentUsage,
                            "reservation_bytes":non_local.CurrentReservation,"budget_bytes":non_local.Budget}}));
                }
                Err(error) => {
                    complete = false;
                    adapters.push(
                        serde_json::json!({"index":index,"name":name,"available":false,
                        "error":error.to_string()}),
                    );
                }
            }
        }
        Ok(
            serde_json::json!({"available":measured>0,"complete":complete && measured>0,
            "scope":"current-process-adapter-node-0","measured_adapters":measured,"adapters":adapters,
            "local_usage_bytes":local_usage,"non_local_usage_bytes":non_local_usage,
            "local_reservation_bytes":local_reservation,"non_local_reservation_bytes":non_local_reservation,
            "local_usage_mib":local_usage as f64/1_048_576.,
            "non_local_usage_mib":non_local_usage as f64/1_048_576.,
            "atlas_image_id_residency_measured":false}),
        )
    })();
    result.unwrap_or_else(|error| {
        serde_json::json!({"available":false,
        "scope":"current-process-adapter-node-0","error":error.to_string()})
    })
}
