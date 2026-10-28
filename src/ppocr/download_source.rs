//! OCR 资产多来源选择：镜像与官方源各取开头一小段测速，按“首包延迟 + 按实测速率下完整个
//! 文件所需时间”排序，下载失败时依次换下一个来源。测速结果按主机在进程内缓存，同一次准备的
//! 后续文件不再重复测速；某主机下载失败后标记为不可用，后续文件先走其它来源。
use crate::{recognition::task::TaskToken, Result};
use std::{
    io::{self, Read},
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Mutex,
    },
    time::{Duration, Instant},
};

/// 测速只取开头 256 KiB；足够越过握手与重定向，又不明显拖慢首次识别。
const PROBE_BYTES: usize = 256 * 1024;
/// 单个来源测速的总上限（含 DNS、TLS、重定向与正文）；各来源并行测，最坏约等这么久。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TTL: Duration = Duration::from_secs(600);
/// 清单首个来源（镜像）的估计时间按此倍数打折：备用源须明显更快才换，避免测速抖动来回切。
const PRIMARY_PREFERENCE: f64 = 1.25;
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const DNS_WORKER_LIMIT: usize = 4;
static DNS_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct DnsWorkerSlot(&'static AtomicUsize);

impl Drop for DnsWorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 系统 DNS 不能强制打断；超时后只留下有数量上限的纯解析工作，不会继续发起 HTTP。
pub(super) fn resolve_asset_address(addr: &str) -> io::Result<Vec<SocketAddr>> {
    resolve_address_with(addr, DNS_TIMEOUT, &DNS_WORKERS, |address| {
        address.to_socket_addrs().map(Iterator::collect)
    })
}

fn resolve_address_with(
    addr: &str,
    timeout: Duration,
    workers: &'static AtomicUsize,
    lookup: impl FnOnce(&str) -> io::Result<Vec<SocketAddr>> + Send + 'static,
) -> io::Result<Vec<SocketAddr>> {
    if let Ok(address) = addr.parse::<SocketAddr>() {
        return Ok(vec![address]);
    }
    workers
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < DNS_WORKER_LIMIT).then_some(active + 1)
        })
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "OCR DNS 解析并发已达上限"))?;
    let slot = DnsWorkerSlot(workers);
    let address = addr.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    let _worker = std::thread::Builder::new()
        .name("rustshot-asset-dns".into())
        .spawn(move || {
            let result =
                lookup(&address).map_err(|error| io::Error::new(error.kind(), "OCR DNS 解析失败"));
            // 超时只结束接收；槽位必须等实际解析结束才归还。panic 也由 Drop 归还。
            drop(slot);
            let _ = sender.send(result);
        })
        .map_err(|_| io::Error::other("OCR DNS 解析线程无法启动"))?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(io::Error::new(io::ErrorKind::TimedOut, "OCR DNS 解析超时"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("OCR DNS 解析线程提前结束"))
        }
    }
}

/// 一个可下载来源：镜像与官方模型字节相同；运行库官方整包与镜像小包身份不同，各自校验。
#[derive(Clone, Copy, Debug)]
pub(super) struct Source<'a> {
    pub url: &'a str,
    pub name: &'a str,
    pub bytes: u64,
    pub sha256: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Probe {
    Reachable {
        latency: Duration,
        bytes_per_second: f64,
    },
    Unreachable,
}

impl Probe {
    fn estimate(self, bytes: u64) -> Option<f64> {
        match self {
            Probe::Reachable {
                latency,
                bytes_per_second,
            } => Some(latency.as_secs_f64() + bytes as f64 / bytes_per_second.max(1.)),
            Probe::Unreachable => None,
        }
    }
}

static CACHE: Mutex<Vec<(String, Probe, Instant)>> = Mutex::new(Vec::new());

/// 返回来源下标的尝试顺序；只有一个来源时不联网测速。
pub(super) fn order(
    agent: &ureq::Agent,
    sources: &[Source<'_>],
    task: Option<&TaskToken>,
) -> Result<Vec<usize>> {
    if sources.len() <= 1 {
        return Ok((0..sources.len()).collect());
    }
    let mut probes: Vec<Option<Probe>> = sources.iter().map(|source| cached(source.url)).collect();
    let cancelled = || task.is_some_and(|task| task.checkpoint().is_err());
    std::thread::scope(|scope| {
        let workers: Vec<_> = sources
            .iter()
            .zip(&probes)
            .enumerate()
            .filter(|(_, (_, probe))| probe.is_none())
            .map(|(index, (source, _))| {
                let agent = agent.clone();
                let url = source.url;
                (index, scope.spawn(move || probe(&agent, url, &cancelled)))
            })
            .collect();
        for (index, worker) in workers {
            let result = worker.join().unwrap_or(Probe::Unreachable);
            probes[index] = Some(result);
        }
    });
    if let Some(task) = task {
        task.checkpoint()?;
    }
    for (source, probe) in sources.iter().zip(&probes) {
        remember(source.url, probe.unwrap_or(Probe::Unreachable));
    }
    let probes: Vec<Probe> = probes
        .into_iter()
        .map(|probe| probe.unwrap_or(Probe::Unreachable))
        .collect();
    Ok(rank(sources, &probes))
}

/// 选中来源下载失败后调用，后续文件先走其它来源。
pub(super) fn mark_failed(url: &str) {
    remember(url, Probe::Unreachable);
}

/// 可达来源按估计耗时升序（首个来源享受偏好折扣），不可达的按清单顺序排在最后兜底。
pub(super) fn rank(sources: &[Source<'_>], probes: &[Probe]) -> Vec<usize> {
    let mut reachable: Vec<(usize, f64)> = Vec::new();
    let mut unreachable = Vec::new();
    for (index, (source, probe)) in sources.iter().zip(probes).enumerate() {
        match probe.estimate(source.bytes) {
            Some(estimate) if index == 0 => reachable.push((index, estimate / PRIMARY_PREFERENCE)),
            Some(estimate) => reachable.push((index, estimate)),
            None => unreachable.push(index),
        }
    }
    reachable.sort_by(|a, b| a.1.total_cmp(&b.1));
    reachable
        .into_iter()
        .map(|(index, _)| index)
        .chain(unreachable)
        .collect()
}

fn probe(agent: &ureq::Agent, url: &str, cancelled: &dyn Fn() -> bool) -> Probe {
    if cancelled() {
        return Probe::Unreachable;
    }
    let started = Instant::now();
    let Ok(response) = agent
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .set("Range", &format!("bytes=0-{}", PROBE_BYTES - 1))
        .call()
    else {
        return Probe::Unreachable;
    };
    let latency = started.elapsed();
    let body_started = Instant::now();
    let mut reader = response.into_reader().take(PROBE_BYTES as u64);
    let mut buffer = [0u8; 16 * 1024];
    let mut received = 0usize;
    // 超时或断流时用已收部分估速；一个字节都没收到才算不可达。
    while !cancelled() {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => received += count,
        }
    }
    if received == 0 || cancelled() {
        return Probe::Unreachable;
    }
    Probe::Reachable {
        latency,
        bytes_per_second: received as f64 / body_started.elapsed().as_secs_f64().max(0.001),
    }
}

/// 缓存键为 scheme://host[:port]；路径与查询串不参与，也不写入日志或错误。
fn host(url: &str) -> &str {
    let rest = url.find("://").map_or(0, |index| index + 3);
    let end = url[rest..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |index| rest + index);
    &url[..end]
}

fn cached(url: &str) -> Option<Probe> {
    let cache = CACHE.lock().ok()?;
    let host = host(url);
    cache
        .iter()
        .find(|(key, _, at)| key == host && at.elapsed() < PROBE_TTL)
        .map(|(_, probe, _)| *probe)
}

fn remember(url: &str, probe: Probe) {
    let Ok(mut cache) = CACHE.lock() else {
        return;
    };
    let host = host(url);
    cache.retain(|(key, _, at)| key != host && at.elapsed() < PROBE_TTL);
    cache.push((host.to_owned(), probe, Instant::now()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_dns_accepts_numeric_addresses_and_lookup_results() {
        static WORKERS: AtomicUsize = AtomicUsize::new(0);
        for numeric in ["127.0.0.1:443", "[::1]:443"] {
            let result = resolve_address_with(numeric, Duration::from_secs(1), &WORKERS, |_| {
                panic!("数字地址不能创建 DNS 工作")
            })
            .unwrap();
            assert_eq!(result, [numeric.parse::<SocketAddr>().unwrap()]);
        }
        let expected: SocketAddr = "127.0.0.1:443".parse().unwrap();
        let result = resolve_address_with(
            "model.example:443",
            Duration::from_secs(1),
            &WORKERS,
            move |_| Ok(vec![expected]),
        )
        .unwrap();
        assert_eq!(result, [expected]);
        assert_eq!(WORKERS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn bounded_dns_sanitizes_lookup_errors() {
        static WORKERS: AtomicUsize = AtomicUsize::new(0);
        let error = resolve_address_with(
            "model.example:443",
            Duration::from_secs(1),
            &WORKERS,
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "private-host:secret",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), "OCR DNS 解析失败");
        assert_eq!(WORKERS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn timed_out_dns_workers_hold_capacity_until_lookup_finishes() {
        static WORKERS: AtomicUsize = AtomicUsize::new(0);
        let mut releases = Vec::new();
        for _ in 0..DNS_WORKER_LIMIT {
            let (release, wait) = mpsc::channel();
            let (started, entered) = mpsc::channel();
            let error = resolve_address_with(
                "model.example:443",
                Duration::from_millis(10),
                &WORKERS,
                move |_| {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(vec!["127.0.0.1:443".parse().unwrap()])
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            entered.recv_timeout(Duration::from_secs(1)).unwrap();
            releases.push(release);
        }
        assert_eq!(WORKERS.load(Ordering::Acquire), DNS_WORKER_LIMIT);
        let error = resolve_address_with(
            "model.example:443",
            Duration::from_secs(1),
            &WORKERS,
            |_| panic!("并发满时不能再创建 DNS 工作"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        for release in releases {
            release.send(()).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while WORKERS.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "已结束的 DNS 工作未归还槽位");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(resolve_address_with(
            "model.example:443",
            Duration::from_secs(1),
            &WORKERS,
            |_| Ok(vec!["127.0.0.1:443".parse().unwrap()]),
        )
        .is_ok());
    }

    fn source(bytes: u64) -> Source<'static> {
        Source {
            url: "https://example.invalid/a",
            name: "a",
            bytes,
            sha256: "",
        }
    }

    fn reachable(latency_ms: u64, mib_per_second: f64) -> Probe {
        Probe::Reachable {
            latency: Duration::from_millis(latency_ms),
            bytes_per_second: mib_per_second * 1024. * 1024.,
        }
    }

    #[test]
    fn faster_alternative_wins_only_when_clearly_faster() {
        let sources = [source(20 << 20), source(20 << 20)];
        // 镜像 2 MiB/s（约 10s），官方 10 MiB/s（约 2s）：换官方源。
        assert_eq!(
            rank(&sources, &[reachable(50, 2.), reachable(300, 10.)]),
            [1, 0]
        );
        // 官方只快约 10%：留在镜像。
        assert_eq!(
            rank(&sources, &[reachable(50, 10.), reachable(50, 11.)]),
            [0, 1]
        );
    }

    #[test]
    fn estimate_uses_each_source_size() {
        // 官方运行库整包是镜像小包的 14 倍：速率略快也不该换。
        let sources = [source(5 << 20), source(71 << 20)];
        assert_eq!(
            rank(&sources, &[reachable(50, 4.), reachable(50, 8.)]),
            [0, 1]
        );
    }

    #[test]
    fn unreachable_sources_stay_last_as_fallback() {
        let sources = [source(1 << 20), source(1 << 20), source(1 << 20)];
        let probes = [Probe::Unreachable, reachable(100, 1.), Probe::Unreachable];
        assert_eq!(rank(&sources, &probes), [1, 0, 2]);
        assert_eq!(
            rank(&sources, &[Probe::Unreachable; 3]),
            [0, 1, 2],
            "全部测速失败时仍按清单顺序尝试"
        );
    }

    #[test]
    fn host_key_drops_path_and_query() {
        assert_eq!(
            host("https://s3.example.net/bucket/a.onnx?x=1"),
            "https://s3.example.net"
        );
        assert_eq!(host("http://127.0.0.1:8080?q"), "http://127.0.0.1:8080");
        assert_eq!(host("https://h.example"), "https://h.example");
    }
}
