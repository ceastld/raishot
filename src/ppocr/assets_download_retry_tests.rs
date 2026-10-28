//! 真实 loopback HTTP、实际下载发布路径与 TaskToken；不访问外网或运行缓存。
use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};
use std::{
    io,
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn qa_root() -> PathBuf {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(workspace.is_absolute());
        workspace.join("target/qa/ppocr-download-transient-retry-20261011/draft/test-temp")
    }

    fn new() -> Self {
        let root = Self::qa_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "{}-{}",
            std::process::id(),
            DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn assert_empty(&self) {
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let root = fs::canonicalize(Self::qa_root()).unwrap();
        let path = fs::canonicalize(&self.0).unwrap();
        assert!(path != root && path.starts_with(root));
        let _ = fs::remove_dir_all(path);
    }
}

enum Reply {
    CloseBeforeHeaders,
    Http(u16, usize, &'static [u8]),
    CancelBeforeHeaders(Arc<CancelOwner>),
    // 仅接受 TCP；TLSConnector 拒绝连接后不会发送 HTTP 请求。
    AcceptOnly,
}

#[derive(Default)]
struct Counts {
    tcp: AtomicUsize,
    requests: AtomicUsize,
}

struct Server {
    url: String,
    counts: Arc<Counts>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<std::result::Result<(), String>>>,
}

impl Server {
    fn new(replies: Vec<Reply>) -> Self {
        assert!(!replies.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/asset?test-secret=redacted-test-query",
            listener.local_addr().unwrap()
        );
        let counts = Arc::new(Counts::default());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_counts = counts.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            while !worker_stop.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    return Err("loopback 服务总等待超时".into());
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => return Err(format!("loopback accept：{:?}", error.kind())),
                };
                let index = worker_counts.tcp.fetch_add(1, Ordering::AcqRel);
                let reply = replies
                    .get(index)
                    .unwrap_or_else(|| replies.last().unwrap());
                if matches!(reply, Reply::AcceptOnly) {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                read_request_headers(&mut stream)?;
                worker_counts.requests.fetch_add(1, Ordering::AcqRel);
                match reply {
                    Reply::CloseBeforeHeaders => {}
                    Reply::CancelBeforeHeaders(owner) => {
                        owner.cancel().map_err(|_| "loopback 取消失败")?;
                    }
                    Reply::Http(status, length, body) => {
                        let headers = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                        );
                        let mut response = headers.into_bytes();
                        response.extend_from_slice(body);
                        write_response(&mut stream, &response)?;
                    }
                    Reply::AcceptOnly => unreachable!(),
                }
                let _ = stream.shutdown(Shutdown::Both);
            }
            Ok(())
        });
        Self {
            url,
            counts,
            stop,
            worker: Some(worker),
        }
    }

    fn wait_for_tcp(&self, minimum: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.counts.tcp.load(Ordering::Acquire) < minimum {
            assert!(Instant::now() < deadline, "loopback TCP 事件未到达");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn finish(mut self) -> (usize, usize) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
        (
            self.counts.tcp.load(Ordering::Acquire),
            self.counts.requests.load(Ordering::Acquire),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // accept 非阻塞、每次读写有界；服务自身另有 8 秒上限。
            let _ = worker.join();
        }
    }
}

fn read_request_headers(stream: &mut TcpStream) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut headers = Vec::new();
    while headers.len() < 8192 {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("loopback 请求总读取超时")?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| "loopback read timeout 配置失败")?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(1) => headers.push(byte[0]),
            Ok(_) => return Err("loopback 请求 headers 提前关闭".into()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("loopback 请求读取：{:?}", error.kind())),
        }
        if headers.ends_with(b"\r\n\r\n") {
            assert!(headers.starts_with(b"GET /asset?"));
            return Ok(());
        }
    }
    Err("loopback 请求 headers 超出固定上限".into())
}

fn write_response(stream: &mut TcpStream, mut bytes: &[u8]) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(500);
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("loopback 响应总写入超时")?;
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|_| "loopback write timeout 配置失败")?;
        match stream.write(bytes) {
            Ok(0) => return Err("loopback 响应写入零字节".into()),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("loopback 响应写入：{:?}", error.kind())),
        }
    }
    Ok(())
}

fn agent_builder() -> ureq::AgentBuilder {
    // QA 明确直连人工 loopback；不会修改或替换产品代理选择。
    ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_millis(500))
        .timeout_read(Duration::from_millis(500))
        .timeout(Duration::from_secs(2))
}

fn asset(url: &str) -> Asset {
    Asset {
        name: "fixed.txt".into(),
        url: url.into(),
        bytes: 3,
        sha256: ABC_SHA.into(),
        upstream_url: None,
        upstream_bytes: None,
        upstream_sha256: None,
    }
}

fn assert_sanitized(error: &(dyn std::error::Error + 'static)) {
    let text = error.to_string();
    assert!(!text.contains("redacted-test-query"));
    assert!(!text.contains("127.0.0.1"));
}

#[test]
fn headers_transient_failure_retries_and_publishes_verified_asset() {
    let directory = TestDirectory::new();
    let server = Server::new(vec![Reply::CloseBeforeHeaders, Reply::Http(200, 3, b"abc")]);
    let owner = CancelOwner::new();
    let path = download_with_agent_cancellable(
        &directory.0,
        &asset(&server.url),
        Some(&owner.token()),
        || Ok(agent_builder().build()),
    )
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abc");
    assert!(verified(&path, 3, ABC_SHA).unwrap());
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    assert_eq!(server.finish(), (2, 2));
}

#[test]
fn http_status_and_body_failures_never_retry_or_publish() {
    // 收到 Response 后分别覆盖状态、固定大小、固定 hash 和流读取失败。
    for reply in [
        Reply::Http(404, 3, b"abc"),
        Reply::Http(500, 3, b"abc"),
        Reply::Http(200, 2, b"ab"),
        Reply::Http(200, 4, b"abcd"),
        Reply::Http(200, 3, b"abd"),
        Reply::Http(200, 3, b"a"),
    ] {
        let directory = TestDirectory::new();
        let server = Server::new(vec![reply]);
        let owner = CancelOwner::new();
        let error = download_with_agent_cancellable(
            &directory.0,
            &asset(&server.url),
            Some(&owner.token()),
            || Ok(agent_builder().build()),
        )
        .unwrap_err();
        assert!(!is_cancelled(error.as_ref()));
        assert_sanitized(error.as_ref());
        directory.assert_empty();
        assert_eq!(server.finish(), (1, 1));
    }
}

#[test]
fn headers_transient_failure_stops_after_three_calls() {
    let directory = TestDirectory::new();
    let server = Server::new(vec![Reply::CloseBeforeHeaders]);
    let owner = CancelOwner::new();
    let error = download_with_agent_cancellable(
        &directory.0,
        &asset(&server.url),
        Some(&owner.token()),
        || Ok(agent_builder().build()),
    )
    .unwrap_err();
    assert!(!is_cancelled(error.as_ref()));
    assert_sanitized(error.as_ref());
    directory.assert_empty();
    assert_eq!(server.finish(), (3, 3));
}

#[test]
fn cancellation_during_headers_stops_followup_requests_and_publication() {
    let directory = TestDirectory::new();
    let owner = Arc::new(CancelOwner::new());
    let server = Server::new(vec![
        Reply::CancelBeforeHeaders(owner.clone()),
        Reply::Http(200, 3, b"abc"),
    ]);
    let error = download_with_agent_cancellable(
        &directory.0,
        &asset(&server.url),
        Some(&owner.token()),
        || Ok(agent_builder().build()),
    )
    .unwrap_err();
    assert!(is_cancelled(error.as_ref()));
    directory.assert_empty();
    assert_eq!(server.finish(), (1, 1));
}

#[test]
fn pre_cancelled_download_has_zero_agent_factory_and_network_calls() {
    let directory = TestDirectory::new();
    let server = Server::new(vec![Reply::Http(200, 3, b"abc")]);
    let factories = AtomicUsize::new(0);
    let owner = CancelOwner::new();
    owner.cancel().unwrap();
    let error = download_with_agent_cancellable(
        &directory.0,
        &asset(&server.url),
        Some(&owner.token()),
        || {
            factories.fetch_add(1, Ordering::AcqRel);
            Ok(agent_builder().build())
        },
    )
    .unwrap_err();
    assert!(is_cancelled(error.as_ref()));
    assert_eq!(factories.load(Ordering::Acquire), 0);
    let wait_error =
        wait_for_download_retry(Duration::from_millis(250), Some(&owner.token())).unwrap_err();
    assert!(is_cancelled(wait_error.as_ref()));
    directory.assert_empty();
    assert_eq!(server.finish(), (0, 0));
}

#[test]
fn verified_cache_has_zero_agent_factory_and_network_calls() {
    let directory = TestDirectory::new();
    let server = Server::new(vec![Reply::Http(200, 3, b"abc")]);
    let fixed = directory.0.join("fixed.txt");
    fs::write(&fixed, b"abc").unwrap();
    let factories = AtomicUsize::new(0);
    let owner = CancelOwner::new();
    let path = download_with_agent_cancellable(
        &directory.0,
        &asset(&server.url),
        Some(&owner.token()),
        || {
            factories.fetch_add(1, Ordering::AcqRel);
            Ok(agent_builder().build())
        },
    )
    .unwrap();
    assert_eq!(path, fixed);
    assert_eq!(fs::read(&path).unwrap(), b"abc");
    assert_eq!(factories.load(Ordering::Acquire), 0);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    assert_eq!(server.finish(), (0, 0));
}

struct RejectingTls {
    kind: io::ErrorKind,
    calls: Arc<AtomicUsize>,
}

impl ureq::TlsConnector for RejectingTls {
    fn connect(
        &self,
        _: &str,
        _: Box<dyn ureq::ReadWrite>,
    ) -> std::result::Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(io::Error::new(self.kind, "人工 TLS connector 拒绝；测试永久错误边界").into())
    }
}

#[test]
fn tls_connector_validation_and_permanent_io_rejections_never_retry() {
    // 真实 HTTPS call/TCP；connector 注入拒绝，不声称已验证真实证书链。
    for kind in [
        io::ErrorKind::InvalidData,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::Other,
    ] {
        let directory = TestDirectory::new();
        let server = Server::new(vec![Reply::AcceptOnly]);
        let calls = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(RejectingTls {
            kind,
            calls: calls.clone(),
        });
        let url = server.url.replacen("http://", "https://", 1);
        let owner = CancelOwner::new();
        let error = download_with_agent_cancellable(
            &directory.0,
            &asset(&url),
            Some(&owner.token()),
            || Ok(agent_builder().tls_connector(connector).build()),
        )
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_sanitized(error.as_ref());
        directory.assert_empty();
        server.wait_for_tcp(1);
        assert_eq!(server.finish(), (1, 0));
    }
}

fn mirrored_asset(primary: &str, upstream: &str) -> Asset {
    Asset {
        upstream_url: Some(upstream.into()),
        ..asset(primary)
    }
}

#[test]
fn unreachable_primary_is_skipped_after_probe() {
    let directory = TestDirectory::new();
    let primary = Server::new(vec![Reply::Http(500, 3, b"abc")]);
    let upstream = Server::new(vec![Reply::Http(200, 3, b"abc")]);
    let owner = CancelOwner::new();
    let path = download_with_agent_cancellable(
        &directory.0,
        &mirrored_asset(&primary.url, &upstream.url),
        Some(&owner.token()),
        || Ok(agent_builder().build()),
    )
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abc");
    // 主源只被测速一次；备用源测速一次、下载一次。
    assert_eq!(primary.finish(), (1, 1));
    assert_eq!(upstream.finish(), (2, 2));
}

#[test]
fn failed_primary_download_falls_back_to_next_source() {
    let directory = TestDirectory::new();
    let primary = Server::new(vec![
        Reply::Http(200, 3, b"abc"),
        Reply::Http(200, 3, b"abd"),
    ]);
    let upstream = Server::new(vec![
        Reply::Http(500, 3, b"abc"),
        Reply::Http(200, 3, b"abc"),
    ]);
    let owner = CancelOwner::new();
    let path = download_with_agent_cancellable(
        &directory.0,
        &mirrored_asset(&primary.url, &upstream.url),
        Some(&owner.token()),
        || Ok(agent_builder().build()),
    )
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abc");
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    assert_eq!(primary.finish(), (2, 2));
    assert_eq!(upstream.finish(), (2, 2));
}
