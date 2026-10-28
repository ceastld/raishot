//! OCR 预热档位，以及只含阶段耗时的诊断记录。
//! 诊断不含截图像素或识别文字；已预热且无等待的请求不写盘。
use super::OcrProfile;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use windows::Win32::{
    Foundation::FILETIME,
    System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes, GetThreadTimes},
};

/// 默认只在截图待命进程预热；“始终”同时覆盖贴图宿主；“关闭”首次识别时再加载。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WarmupMode {
    Off,
    #[default]
    Standby,
    Always,
}

impl WarmupMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "standby" => Some(Self::Standby),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Standby => "standby",
            Self::Always => "always",
        }
    }
}

pub(super) fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.
}

fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// 当前线程累计 CPU（内核＋用户）毫秒；与墙钟对比区分“算得慢”和“在等外部”。
pub(super) fn thread_cpu_ms() -> Option<f64> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    Some((filetime_ticks(kernel) + filetime_ticks(user)) as f64 / 10_000.)
}

/// 一次识别或预热的阶段耗时（毫秒）；未发生的阶段为 None。
#[derive(Default)]
pub(super) struct Timing {
    /// 等待进程内引擎锁，通常是在等仍在进行的预热。
    pub lock_wait_ms: f64,
    /// 资产校验，缺文件时含下载。
    pub assets_ms: Option<f64>,
    /// 运行库 DLL 载入与 API 绑定。
    pub runtime_ms: Option<f64>,
    /// ORT 全局环境（算子注册、设备发现）。
    pub environment_ms: Option<f64>,
    /// 同一阶段本线程实际占用的 CPU。
    pub environment_cpu_ms: Option<f64>,
    pub detection_ms: Option<f64>,
    /// 字典与 cls／rec 会话。
    pub recognition_ms: Option<f64>,
    pub run_ms: Option<f64>,
}

impl Timing {
    pub(super) fn add_assets(&mut self, started: Instant) {
        *self.assets_ms.get_or_insert(0.) += elapsed_ms(started);
    }

    fn initialized(&self) -> bool {
        self.runtime_ms.is_some() || self.detection_ms.is_some() || self.recognition_ms.is_some()
    }

    /// 只留下能解释“偶发等很久”的事件：预热、现场初始化、等锁或总耗时异常。
    fn notable(&self, kind: &str, total_ms: f64) -> bool {
        kind == "warmup" || self.initialized() || self.lock_wait_ms >= 100. || total_ms >= 1500.
    }

    pub(super) fn record(&self, kind: &'static str, profile: OcrProfile, started: Instant) {
        let total_ms = elapsed_ms(started);
        if !self.notable(kind, total_ms) {
            return;
        }
        let round = |value: f64| (value * 10.).round() / 10.;
        let stage = |value: Option<f64>| value.map(round);
        let line = serde_json::json!({
            "unix_ms": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis()),
            "pid": std::process::id(),
            "kind": kind,
            "profile": profile.as_str(),
            "process_age_ms": process_age_ms(),
            "lock_wait_ms": round(self.lock_wait_ms),
            "assets_ms": stage(self.assets_ms),
            "runtime_ms": stage(self.runtime_ms),
            "environment_ms": stage(self.environment_ms),
            "environment_cpu_ms": stage(self.environment_cpu_ms),
            "detection_ms": stage(self.detection_ms),
            "recognition_ms": stage(self.recognition_ms),
            "run_ms": stage(self.run_ms),
            "total_ms": round(total_ms),
            // 本进程运行库是否已跳过 SetupAPI 设备枚举（见 `discovery.rs`）。
            "device_scan_skipped": super::runtime::device_scan_skipped(),
        })
        .to_string();
        // 测试不写用户诊断目录；输出由测试框架捕获，测时用例加 --nocapture 查看。
        if cfg!(test) {
            println!("{line}");
            return;
        }
        let Some(directory) = crate::capture_failure::diagnostics_dir() else {
            return;
        };
        if std::fs::create_dir_all(&directory).is_ok() {
            crate::capture_failure::append_history(&directory.join("ocr-timing.jsonl"), &line);
        }
    }
}

/// 进程已存活时长：区分新待命进程与长时间闲置后的首次识别。
fn process_age_ms() -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    let created = filetime_ticks(created);
    // FILETIME 以 1601-01-01 为原点，单位 100ns。
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let now = (now.as_nanos() / 100) as u64 + 11_644_473_600 * 10_000_000;
    Some(now.saturating_sub(created) / 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_mode_round_trips_and_rejects_unknown_values() {
        for mode in [WarmupMode::Off, WarmupMode::Standby, WarmupMode::Always] {
            assert_eq!(WarmupMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(WarmupMode::default(), WarmupMode::Standby);
        assert_eq!(WarmupMode::parse(""), None);
        assert_eq!(WarmupMode::parse("ALWAYS"), None);
    }

    #[test]
    fn only_slow_or_initializing_requests_are_notable() {
        let warm = Timing::default();
        assert!(!warm.notable("recognize", 300.));
        assert!(warm.notable("recognize", 1500.));
        assert!(warm.notable("warmup", 1.));
        let waited = Timing {
            lock_wait_ms: 120.,
            ..Timing::default()
        };
        assert!(waited.notable("detect", 200.));
        let cold = Timing {
            recognition_ms: Some(40.),
            ..Timing::default()
        };
        assert!(cold.notable("recognize", 200.));
    }

    /// 冷预热分段测时（默认忽略，需已缓存所选资产）；每次运行都是新进程。
    /// `RUSTSHOT_OCR_DEVICE_SCAN=full` 保留 ORT 完整设备发现作对照：
    /// `cargo test --lib ppocr::warmup::tests::cold_warmup_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cold_warmup_timing() {
        let owner = crate::recognition::task::CancelOwner::new();
        assert!(super::super::prepare_cached_cancellable(&owner.token()).unwrap());
    }

    #[test]
    fn process_age_is_measured_from_process_creation() {
        let age = process_age_ms().expect("当前进程创建时间");
        assert!(age < 24 * 60 * 60 * 1000, "{age}");
    }
}
