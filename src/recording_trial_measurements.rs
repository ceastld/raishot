//! 短片试验的进程级端点读数；不轮询、启动线程或查询 GPU。
//! CPU 包含同进程所有线程，内存只取两个端点，不能据此归因编码器或推断区间峰值。

use serde_json::{json, Value};
use std::time::Instant;
use windows::Win32::{
    Foundation::FILETIME,
    System::Threading::{GetCurrentProcess, GetProcessTimes},
};

#[derive(Clone, Copy, Debug)]
struct CpuTimes {
    kernel: u64,
    user: u64,
}

#[derive(Debug)]
struct CpuSample {
    times: Option<CpuTimes>,
    error: Option<Value>,
}

impl CpuSample {
    fn read() -> Self {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        match unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } {
            Ok(()) => Self {
                times: Some(CpuTimes {
                    kernel: filetime_ticks(kernel),
                    user: filetime_ticks(user),
                }),
                error: None,
            },
            Err(error) => Self {
                times: None,
                error: Some(json!({"operation": "GetProcessTimes",
                    "message": error.to_string(), "hresult": error.code().0})),
            },
        }
    }

    fn report(&self) -> Value {
        json!({"available": self.times.is_some(),
            "kernel_ticks_100ns": self.times.map(|times| times.kernel),
            "user_ticks_100ns": self.times.map(|times| times.user),
            "error": self.error})
    }
}

fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn cpu_delta_ms(before: CpuTimes, after: CpuTimes) -> Result<f64, &'static str> {
    after
        .kernel
        .checked_sub(before.kernel)
        .ok_or("process kernel CPU ticks moved backwards")?;
    after
        .user
        .checked_sub(before.user)
        .ok_or("process user CPU ticks moved backwards")?;
    let before_total = before
        .kernel
        .checked_add(before.user)
        .ok_or("start process CPU tick total overflowed")?;
    let after_total = after
        .kernel
        .checked_add(after.user)
        .ok_or("end process CPU tick total overflowed")?;
    let ticks = after_total
        .checked_sub(before_total)
        .ok_or("process CPU tick total moved backwards")?;
    // FILETIME 的计数单位为 100 ns；每毫秒 10000 tick，保留小于一毫秒的部分。
    Ok((ticks / 10_000) as f64 + (ticks % 10_000) as f64 / 10_000.)
}

fn memory_sample() -> Value {
    match crate::process_memory::cpu_memory() {
        Ok(memory) => {
            let working_set = memory.get("working_set_bytes").and_then(Value::as_u64);
            let private = memory.get("private_bytes").and_then(Value::as_u64);
            let complete = working_set.is_some() && private.is_some();
            json!({"available": complete, "working_set_bytes": working_set,
                "private_bytes": private,
                "error": (!complete).then(|| json!({"operation": "cpu_memory result",
                    "message": "process memory endpoint fields are missing or invalid"}))})
        }
        Err(error) => {
            let hresult = error
                .downcast_ref::<windows::core::Error>()
                .map(|error| error.code().0);
            json!({"available": false, "working_set_bytes": null, "private_bytes": null,
                "error": {"operation": "cpu_memory/K32GetProcessMemoryInfo",
                    "message": error.to_string(), "hresult": hresult}})
        }
    }
}

#[derive(Debug)]
pub(crate) struct TrialMeasurement {
    started: Instant,
    cpu_before: CpuSample,
    memory_before: Value,
}

impl TrialMeasurement {
    pub(crate) fn start() -> Self {
        let started = Instant::now();
        Self {
            started,
            cpu_before: CpuSample::read(),
            memory_before: memory_sample(),
        }
    }

    pub(crate) fn finish(self) -> Value {
        let cpu_after = CpuSample::read();
        let memory_after = memory_sample();
        let wall_elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.;
        let cpu_elapsed = match (self.cpu_before.times, cpu_after.times) {
            (Some(before), Some(after)) => cpu_delta_ms(before, after),
            _ => Err("process CPU endpoint unavailable; see cpu_before/cpu_after errors"),
        };
        json!({
            "wall_elapsed_ms": wall_elapsed_ms,
            "process_cpu_elapsed_ms": cpu_elapsed.as_ref().ok().copied(),
            "process_cpu_unknown_reason": cpu_elapsed.err(),
            "cpu_before": self.cpu_before.report(), "cpu_after": cpu_after.report(),
            "memory_before": self.memory_before, "memory_after": memory_after,
            "scope": "entire current-process kernel+user CPU and working-set/private-byte endpoints; includes other threads, libraries and trial overhead; wall time brackets endpoint sampling, while CPU and memory are sequential samples; not encoder-exclusive CPU, interval peak memory, GPU utilization or steady-state performance",
            "encoder_exclusive": false,
            "interval_peak_memory_measured": false,
            "gpu_utilization_measured": false,
            "steady_state_performance_proved": false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_trial_measurements_cpu_ticks_are_checked_and_use_100ns_units() {
        let zero = CpuTimes { kernel: 0, user: 0 };
        assert_eq!(cpu_delta_ms(zero, zero), Ok(0.));
        assert_eq!(
            cpu_delta_ms(zero, CpuTimes { kernel: 1, user: 0 }),
            Ok(0.0001)
        );
        assert_eq!(
            cpu_delta_ms(
                CpuTimes {
                    kernel: 10_000,
                    user: 20_000
                },
                CpuTimes {
                    kernel: 15_000,
                    user: 30_000
                }
            ),
            Ok(1.5)
        );
        let before = CpuTimes { kernel: 2, user: 2 };
        for after in [
            CpuTimes { kernel: 1, user: 9 },
            CpuTimes { kernel: 9, user: 1 },
        ] {
            assert!(cpu_delta_ms(before, after).is_err());
        }
        let overflow = CpuTimes {
            kernel: u64::MAX,
            user: 1,
        };
        assert!(cpu_delta_ms(overflow, overflow).is_err());
        assert!(cpu_delta_ms(zero, overflow).is_err());
        assert!(cpu_delta_ms(
            zero,
            CpuTimes {
                kernel: u64::MAX,
                user: 0
            }
        )
        .unwrap()
        .is_finite());
    }
}
