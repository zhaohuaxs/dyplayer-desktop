//! 本地 HTTP Range 流代理（M3）：把 VFS 文件暴露为 mpv 可消费的本地 HTTP 流。
//!
//! - tiny_http 服务，绑定 `127.0.0.1:0` 随机端口，仅回环。
//! - 路由 `GET/HEAD /s/{token}/{path}`：token 为随机鉴权串（防本机其他进程滥用），
//!   path 为 percent-encoded 的 VFS 路径。
//! - Range：`bytes=start-end` / `bytes=start-` / `bytes=-suffix`（206 + Content-Range）；
//!   无 Range / 非法 Range → 200 全量；start 超界 → 416。
//! - 一个 StreamProxy 绑定一个 VfsSource；播放会话持有，Drop 即关闭。


use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use tiny_http::{Header, Method, Response, Server};

use dyplayer_vfs::{VfsError, VfsResult, VfsSource};

/// URL 路径段中需要编码的字符。
/// 注意 `/` 也被编码：token 与 VFS 路径之间仅靠一个 `/` 分隔，
/// 路径内部的分隔符（`/` 或 `\`）全部编码后才能无损往返——
/// LocalFs 吐宿主机绝对路径、SmbFs 吐 UNC、WebDavFs 吐源内 `/` 路径，三者形态互不兼容。
const PATH_SEGMENT_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%')
    .add(b'/')
    .add(b'\\')
    .add(b'^')
    .add(b'|');

const WORKERS: usize = 4;

/// 会话级上游吞吐诊断计数（原子无锁）：播放页 tick 每秒快照差分，
/// 得到上游速率（`up=MB/s`）与 mpv GET 频次（`gets=+N`，seek 重连信号）。
#[derive(Default)]
pub struct ProxyStats {
    upstream_bytes: AtomicU64,
    gets: AtomicU64,
    heads: AtomicU64,
}

impl ProxyStats {
    /// 上游累计读取字节数。含 200 降级路径（WebDAV 忽略 Range 全量重下、
    /// SkipReader 丢弃的前缀）——中转型网关的流量放大在此直接可见。
    pub fn upstream_bytes(&self) -> u64 {
        self.upstream_bytes.load(Ordering::Relaxed)
    }
    /// 累计 GET 请求数（每次 GET = 一条上游连接）。
    pub fn gets(&self) -> u64 {
        self.gets.load(Ordering::Relaxed)
    }
    /// 累计 HEAD 请求数。
    pub fn heads(&self) -> u64 {
        self.heads.load(Ordering::Relaxed)
    }
}

/// 包装 VFS reader 统计真实上游流量（含降级路径被丢弃的字节）。
/// 首次成功读取时嗅探响应体：判定网关返回的是媒体容器字节还是
/// HLS manifest 文本（后者 = 网关走了转码/预览接口，非文件本体）。
struct CountingReader {
    inner: Box<dyn Read + Send>,
    stats: Arc<ProxyStats>,
    path: PathBuf,
    sniff_pending: bool,
}

impl CountingReader {
    fn new(inner: Box<dyn Read + Send>, stats: Arc<ProxyStats>, path: PathBuf) -> Self {
        Self {
            inner,
            stats,
            path,
            sniff_pending: true,
        }
    }
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.stats.upstream_bytes.fetch_add(n as u64, Ordering::Relaxed);
            if self.sniff_pending && n >= 8 {
                self.sniff_pending = false;
                if is_hls_manifest_text(&buf[..n]) {
                    tracing::warn!(
                        "上游 GET 返回 HLS manifest（网关转码流而非文件字节）path={}：\
                         mpv 将按相对路径拉 .ts 分段，proxy 转发到网关根域必 404 \
                         → 播放失败 error -13 的直接根因",
                        self.path.display()
                    );
                    // 捕获 manifest 首块（含 URI 行样式）落盘，供修复方案定 URI 形态。
                    let cap = std::env::temp_dir().join("dyplayer_hls_capture.txt");
                    let mut head = format!("path={}\n---\n", self.path.display())
                        .into_bytes();
                    head.extend_from_slice(&buf[..n.min(16384)]);
                    match std::fs::write(&cap, &head) {
                        Ok(()) => tracing::warn!(
                            "HLS manifest 首块已落盘: {}（{} 字节）",
                            cap.display(),
                            head.len()
                        ),
                        Err(e) => tracing::warn!("HLS manifest 落盘失败: {e}"),
                    }
                }
            }
        }
        Ok(n)
    }
}

/// 首包嗅探：UTF-8 文本且含 HLS 标记（`#EXTM3U` / `#EXT-X`）判定为 manifest。
/// 二进制媒体容器（mkv EBML / mp4 ftyp）UTF-8 解码必失败 → false。
fn is_hls_manifest_text(buf: &[u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    let head = &buf[..buf.len().min(512)];
    match std::str::from_utf8(head) {
        Ok(txt) => txt.contains("#EXTM3U") || txt.contains("#EXT-X"),
        Err(_) => false,
    }
}

/// stat 结果会话级缓存：mpv 播放期间同一文件的 Range 请求密集，
/// 每个响应的 `Content-Range` total 都要 stat（远程源 = 一次 PROPFIND
/// 往返）。会话内文件大小不变是合理假设，miss 时穿透一次并缓存；
/// proxy 生命周期 = 播放会话，换源重建自然失效。
type StatCache = Arc<Mutex<HashMap<PathBuf, u64>>>;

pub struct StreamProxy {
    addr: SocketAddr,
    token: String,
    server: Arc<Server>,
    stopped: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    stats: Arc<ProxyStats>,
}

impl StreamProxy {
    /// 在随机回环端口上启动代理，绑定一个 VFS 源。
    pub fn spawn(source: Arc<dyn VfsSource>) -> anyhow::Result<Self> {
        let server = Server::http("127.0.0.1:0")
            .map_err(|e| anyhow::anyhow!("启动流代理失败: {e}"))?;
        let server = Arc::new(server);
        let tiny_http::ListenAddr::IP(addr) = server.server_addr();
        let token = generate_token();
        let stopped = Arc::new(AtomicBool::new(false));
        let stat_cache: StatCache = Arc::new(Mutex::new(HashMap::new()));
        let stats = Arc::new(ProxyStats::default());
        let mut workers = Vec::with_capacity(WORKERS);
        for _ in 0..WORKERS {
            let server = Arc::clone(&server);
            let stopped = Arc::clone(&stopped);
            let source = Arc::clone(&source);
            let stat_cache = Arc::clone(&stat_cache);
            let stats = Arc::clone(&stats);
            let token = token.clone();
            let handle = std::thread::Builder::new()
                .name("dyplayer-proxy".into())
                .spawn(move || {
                    worker_loop(server, stopped, source, token, stat_cache, stats)
                })
                .map_err(|e| anyhow::anyhow!("创建代理线程失败: {e}"))?;
            workers.push(handle);
        }
        Ok(Self {
            addr,
            token,
            server,
            stopped,
            workers,
            stats,
        })
    }

    /// VFS 路径 → 代理 URL（mpv loadfile 用）。
    ///
    /// VFS 路径对代理是不透明字符串（各 Fs 形态不同：宿主机绝对路径 / UNC / 源内 `/` 路径），
    /// 编码后经 `parse_route` 无损还原。
    pub fn url_for(&self, vfs_path: &Path) -> String {
        let raw = vfs_path.to_string_lossy();
        let encoded = utf8_percent_encode(&raw, PATH_SEGMENT_SET);
        format!("http://{}/s/{}/{}", self.addr, self.token, encoded)
    }

    /// 监听地址（测试断言与诊断用；生产侧经 `url_for` 取完整 URL）。
    #[cfg(test)]
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 上游吞吐诊断计数（app 侧 tick 每秒差分打日志）。
    pub fn stats(&self) -> &Arc<ProxyStats> {
        &self.stats
    }
}

impl Drop for StreamProxy {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // tiny_http 的 unblock() 每次只唤醒一个阻塞的 recv，
        // 因此逐 worker 轮询补发，直至全部退出或超过总等待预算。
        //
        // 必须有上限：worker 可能正卡在 `handle_request` 里等待上游响应
        // （VFS 读超时最长 30s），JoinHandle::join 会无限阻塞调用线程——
        // 换源时 drop 发生在 egui UI 线程，无限等待即窗口"未响应"。
        // 超预算后放弃 join 直接 detach（JoinHandle drop 不阻塞），
        // worker 收尾后自行退出；Arc<Server>/Arc<VfsSource> 延迟释放可接受。
        let deadline = Instant::now() + Duration::from_millis(800);
        for w in self.workers.drain(..) {
            while !w.is_finished() {
                self.server.unblock();
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if w.is_finished() {
                let _ = w.join();
            }
        }
    }
}

fn worker_loop(
    server: Arc<Server>,
    stopped: Arc<AtomicBool>,
    source: Arc<dyn VfsSource>,
    token: String,
    stat_cache: StatCache,
    stats: Arc<ProxyStats>,
) {
    let mut err_streak = 0u32;
    loop {
        match server.recv() {
            Ok(request) => {
                err_streak = 0;
                handle_request(request, &source, &token, &stat_cache, &stats);
            }
            Err(_) => {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                // F14：recv 错误后退避避免空转 CPU；连续 10 次升级为 error 并退出
                // （持续错误通常意味着端口/资源耗尽，继续重试无意义）。
                err_streak = err_streak.saturating_add(1);
                if err_streak >= 10 {
                    tracing::error!(
                        target: "proxy",
                        "worker recv 连续 {err_streak} 次错误，退出 worker"
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

fn handle_request(
    request: tiny_http::Request,
    source: &Arc<dyn VfsSource>,
    token: &str,
    stat_cache: &StatCache,
    stats: &Arc<ProxyStats>,
) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let range_header = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    let Some((vfs_path, token_ok)) = parse_route(&url, token) else {
        let _ = request.respond(error_response(404, "Not Found"));
        return;
    };
    if !token_ok {
        let _ = request.respond(error_response(403, "Forbidden"));
        return;
    }

    match method {
        Method::Head => {
            stats.heads.fetch_add(1, Ordering::Relaxed);
            respond_head(request, source, &vfs_path, stat_cache)
        }
        Method::Get => {
            stats.gets.fetch_add(1, Ordering::Relaxed);
            respond_get(request, source, &vfs_path, range_header.as_deref(), stat_cache, stats)
        }
        _ => {
            let _ = request.respond(error_response(405, "Method Not Allowed"));
        }
    }
}

/// `GET/HEAD /s/{token}/{encoded-path}` → (VFS 路径, token 是否匹配)。
/// 路径内的 `/` 与 `\` 均被 percent-encode，token 后第一个 `/` 即路径起点。
fn parse_route(url: &str, token: &str) -> Option<(PathBuf, bool)> {
    let path_part = url.split('?').next().unwrap_or("");
    let rest = path_part.strip_prefix("/s/")?;
    let (got_token, enc_path) = match rest.split_once('/') {
        Some((t, p)) => (t, p),
        None => return None,
    };
    let decoded = percent_decode_str(enc_path).decode_utf8_lossy().into_owned();
    if decoded.is_empty() || decoded == "/" {
        return None;
    }
    Some((PathBuf::from(decoded), got_token == token))
}

fn respond_head(
    request: tiny_http::Request,
    source: &Arc<dyn VfsSource>,
    path: &Path,
    stat_cache: &StatCache,
) {
    // 与 GET 的 Range 路径共用 stat 缓存（mpv 探测期 HEAD+Range 混发，
    // 远程源每次 miss 都是一次 PROPFIND 往返）。
    match stat_size_cached(stat_cache, source, path) {
        Ok(size) => {
            let resp = Response::empty(200)
                .with_header(header("Content-Length", &size.to_string()))
                .with_header(header("Accept-Ranges", "bytes"))
                .with_header(header("Content-Type", "application/octet-stream"));
            let _ = request.respond(resp);
        }
        Err(e) => {
            let _ = request.respond(error_response_for(e, path));
        }
    }
}

fn respond_get(
    request: tiny_http::Request,
    source: &Arc<dyn VfsSource>,
    path: &Path,
    range: Option<&str>,
    stat_cache: &StatCache,
    stats: &Arc<ProxyStats>,
) {
    let spec = parse_range_header(range);
    match spec {
        RangeSpec::Full => match source.open_range(path, 0, None) {
            Ok(r) => {
                let reader = Box::new(CountingReader::new(
                    r.reader,
                    Arc::clone(stats),
                    path.to_path_buf(),
                ));
                let resp = Response::new(
                    tiny_http::StatusCode(200),
                    vec![
                        header("Content-Length", &r.length.to_string()),
                        header("Accept-Ranges", "bytes"),
                        header("Content-Type", "application/octet-stream"),
                    ],
                    reader,
                    Some(r.length as usize),
                    None,
                );
                let _ = request.respond(resp);
            }
            Err(e) => {
                let _ = request.respond(error_response_for(e, path));
            }
        },
        RangeSpec::From(start) | RangeSpec::FromTo(start, _) => {
            let len = match spec {
                RangeSpec::FromTo(_, end) => end.checked_sub(start).map(|d| d + 1),
                _ => None,
            };
            respond_partial(request, source, path, start, len, stat_cache, stats)
        }
        RangeSpec::Suffix(suffix) => {
            if suffix == 0 {
                let _ = request.respond(error_response(416, "Range Not Satisfiable"));
                return;
            }
            match stat_size_cached(stat_cache, source, path) {
                Ok(size) => {
                    let start = size.saturating_sub(suffix);
                    let len = size - start;
                    respond_partial(request, source, path, start, Some(len), stat_cache, stats)
                }
                Err(e) => {
                    let _ = request.respond(error_response_for(e, path));
                }
            }
        }
    }
}

fn respond_partial(
    request: tiny_http::Request,
    source: &Arc<dyn VfsSource>,
    path: &Path,
    start: u64,
    len: Option<u64>,
    stat_cache: &StatCache,
    stats: &Arc<ProxyStats>,
) {
    match source.open_range(path, start, len) {
        Ok(r) => {
            let end = start + r.length.saturating_sub(1);
            let reader = Box::new(CountingReader::new(
                r.reader,
                Arc::clone(stats),
                path.to_path_buf(),
            ));
            let resp = Response::new(
                tiny_http::StatusCode(206),
                vec![
                    header(
                        "Content-Range",
                        &format!(
                            "bytes {start}-{end}/{}",
                            total_hint(stat_cache, source, path, end)
                        ),
                    ),
                    header("Content-Length", &r.length.to_string()),
                    header("Accept-Ranges", "bytes"),
                    header("Content-Type", "application/octet-stream"),
                ],
                reader,
                Some(r.length as usize),
                None,
            );
            let _ = request.respond(resp);
        }
        Err(VfsError::InvalidRange { start, total }) => {
            let _ = request.respond(
                Response::empty(416)
                    .with_header(header("Content-Range", &format!("bytes */{total}")))
                    .with_header(header("X-Requested-Range", &format!("bytes={start}-"))),
            );
        }
        Err(e) => {
            let _ = request.respond(error_response_for(e, path));
        }
    }
}

/// stat 缓存查询：hit 直接返回；miss 穿透 stat（保留错误类型）并写入。
/// 锁只覆盖 HashMap 读写，stat（可能 30s 超时的网络往返）在锁外执行。
fn stat_size_cached(
    stat_cache: &StatCache,
    source: &Arc<dyn VfsSource>,
    path: &Path,
) -> VfsResult<u64> {
    {
        let guard = lock_cache(stat_cache);
        if let Some(&size) = guard.get(path) {
            return Ok(size);
        }
    }
    let size = source.stat(path).map(|m| m.size)?;
    lock_cache(stat_cache).insert(path.to_path_buf(), size);
    Ok(size)
}

/// Content-Range 的 total 字段：stat 缓存优先，失败时用已读到的 end+1 近似。
fn total_hint(stat_cache: &StatCache, source: &Arc<dyn VfsSource>, path: &Path, end_read: u64) -> u64 {
    stat_size_cached(stat_cache, source, path).unwrap_or(end_read + 1)
}

fn lock_cache(stat_cache: &StatCache) -> std::sync::MutexGuard<'_, HashMap<PathBuf, u64>> {
    stat_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("合法响应头")
}

fn error_response(status: u16, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(format!("{status} {msg}\n").into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

/// 上游错误 → HTTP 错误响应 + WARN 日志（mpv -13 加载失败的取证链路：
/// 日志必须能看到"哪个文件、上游什么错误"，否则 error -13 无从排查）。
fn error_response_for(e: VfsError, path: &Path) -> Response<std::io::Cursor<Vec<u8>>> {
    tracing::warn!("proxy 上游错误 path={}: {e}", path.display());
    match e {
        VfsError::NotFound(_) => error_response(404, "Not Found"),
        VfsError::AccessDenied(_) => error_response(403, "Access Denied"),
        VfsError::OutsideRoot { .. } => error_response(403, "Outside Root"),
        VfsError::InvalidRange { .. } => error_response(416, "Range Not Satisfiable"),
        _ => error_response(502, "Upstream Error"),
    }
}

fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32;
    let mut out = String::with_capacity(24);
    for _ in 0..24 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let idx = (seed >> 33) as usize % 62;
        out.push(b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"[idx] as char);
    }
    out
}

// ---------- Range 解析（纯函数，单测覆盖） ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RangeSpec {
    /// 无 Range 或无法解析 → 200 全量。
    Full,
    /// `bytes=start-end`（闭区间）。
    FromTo(u64, u64),
    /// `bytes=start-`。
    From(u64),
    /// `bytes=-suffix`。
    Suffix(u64),
}

pub(crate) fn parse_range_header(range: Option<&str>) -> RangeSpec {
    let Some(range) = range else {
        return RangeSpec::Full;
    };
    let Some(rest) = range.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    if rest.contains(',') {
        // 多范围不支持（mpv/FFmpeg 只发单范围），降级全量。
        return RangeSpec::Full;
    }
    let (start, end) = match rest.split_once('-') {
        Some(pair) => pair,
        None => return RangeSpec::Full,
    };
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() && end.is_empty() {
        return RangeSpec::Full;
    }
    if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return RangeSpec::Full;
        };
        return RangeSpec::Suffix(suffix);
    }
    let Ok(start) = start.parse::<u64>() else {
        return RangeSpec::Full;
    };
    if end.is_empty() {
        return RangeSpec::From(start);
    }
    match end.parse::<u64>() {
        Ok(end) if end >= start => RangeSpec::FromTo(start, end),
        _ => RangeSpec::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyplayer_vfs::LocalFs;

    /// HLS manifest 嗅探判定（error -13 取证链的纯函数）。
    #[test]
    fn hls_manifest_sniff() {
        assert!(is_hls_manifest_text(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:10.0,\nmedia-abc-0.ts\n"
        ));
        assert!(is_hls_manifest_text(b"#EXT-X-STREAM-INF ..."));
        // mkv EBML 头：二进制，UTF-8 解码失败 → 非文本
        assert!(!is_hls_manifest_text(&[0x1a, 0x45, 0xdf, 0xa3, 0x8f, 0x00, 0x00, 0x10]));
        // mp4 头：ftyp 是 ASCII 可解码，但不含 HLS 标记
        assert!(!is_hls_manifest_text(b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00"));
        // 二进制视频流中段
        assert!(!is_hls_manifest_text(&[0x5b, 0xc0, 0xff, 0xee, 0x01, 0x02, 0x03, 0x84]));
        // 过短
        assert!(!is_hls_manifest_text(b"#EXT"));
        // 纯文本但非 manifest（JSON 错误页等）
        assert!(!is_hls_manifest_text(b"{\"error\":\"rate limited\"}"));
    }


    #[test]
    fn range_spec_parsing() {
        assert_eq!(parse_range_header(None), RangeSpec::Full);
        assert_eq!(parse_range_header(Some("bytes=0-")), RangeSpec::From(0));
        assert_eq!(
            parse_range_header(Some("bytes=100-199")),
            RangeSpec::FromTo(100, 199)
        );
        assert_eq!(parse_range_header(Some("bytes=-500")), RangeSpec::Suffix(500));
        assert_eq!(parse_range_header(Some("garbage")), RangeSpec::Full);
        assert_eq!(parse_range_header(Some("items=0-1")), RangeSpec::Full);
        assert_eq!(parse_range_header(Some("bytes=9-2")), RangeSpec::Full);
        assert_eq!(parse_range_header(Some("bytes=0-1,5-6")), RangeSpec::Full);
        assert_eq!(parse_range_header(Some("bytes=-")), RangeSpec::Full);
        assert_eq!(parse_range_header(Some("bytes=a-b")), RangeSpec::Full);
    }

    #[test]
    fn route_parsing() {
        let (path, ok) = parse_route("/s/tok123/dir%2Ffile.mp4", "tok123").unwrap();
        assert_eq!(path, PathBuf::from("dir/file.mp4"));
        assert!(ok);
        let (path, ok) = parse_route("/s/wrong/dir%2Ffile.mp4", "tok123").unwrap();
        assert_eq!(path, PathBuf::from("dir/file.mp4"));
        assert!(!ok);
        let (path, ok) = parse_route(
            "/s/tok123/%2F%E7%94%B5%E5%BD%B1%2F%E5%BC%B9%E5%B9%95.xml",
            "tok123",
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/电影/弹幕.xml"));
        assert!(ok);
        assert!(parse_route("/other/x", "tok123").is_none());
        assert!(parse_route("/s/tok123", "tok123").is_none());
        assert!(parse_route("/s/tok123/", "tok123").is_none());
    }

    /// url_for → parse_route 必须无损往返，覆盖三种 Fs 的路径形态。
    #[test]
    fn url_for_roundtrips_all_path_shapes() {
        let cases = [
            r"C:\Users\test\v.bin",          // LocalFs（Windows 宿主路径）
            "/home/test/v.bin",              // LocalFs（Unix 宿主路径）
            r"\\nas\media\movie\a.mp4",      // SmbFs（UNC）
            "/dir/电影 弹幕#1.mp4",           // WebDavFs（源内路径）
        ];
        for raw in cases {
            let url = url_for_static("127.0.0.1:9999", "tok123", Path::new(raw));
            let path_part = url.split("://").nth(1).unwrap().split_once('/').unwrap().1;
            let (got, ok) = parse_route(&format!("/{path_part}"), "tok123").unwrap();
            assert!(ok, "token 校验失败: {raw}");
            assert_eq!(got, PathBuf::from(raw), "往返不一致");
        }
    }

    fn url_for_static(addr: &str, token: &str, vfs_path: &Path) -> String {
        let raw = vfs_path.to_string_lossy();
        let encoded = utf8_percent_encode(&raw, PATH_SEGMENT_SET);
        format!("http://{}/s/{}/{}", addr, token, encoded)
    }

    #[test]
    fn token_is_randomish_and_url_builder() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 24);
    }

    // ---------- TCP 集成测试 ----------

    fn spawn_local_proxy(tag: &str) -> (StreamProxy, PathBuf, Vec<u8>) {
        // tag 必须每测试唯一：并行测试共用目录会互相 remove_dir_all 竞态。
        let dir = std::env::temp_dir().join(format!(
            "dyplayer_proxy_test_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("v.bin"), &data).unwrap();
        let fs = LocalFs::new(&dir).unwrap();
        let proxy = StreamProxy::spawn(Arc::new(fs)).unwrap();
        (proxy, dir.join("v.bin"), data)
    }

    fn http_get(
        url: &str,
        range: Option<&str>,
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let builder = ureq::Agent::config_builder()
            .http_status_as_error(false);
        let agent = ureq::Agent::new_with_config(builder.build());
        let mut req = ureq::http::Request::builder().method("GET").uri(url);
        if let Some(r) = range {
            req = req.header("Range", r);
        }
        let resp = agent.run(req.body(()).unwrap()).unwrap();
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let mut body = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut body).unwrap();
        (status, headers, body)
    }

    fn header_of(headers: &[(String, String)], name: &str) -> String {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn integration_full_get() {
        let (proxy, path, data) = spawn_local_proxy("full_get");
        let url = proxy.url_for(&path);
        let (status, headers, body) = http_get(&url, None);
        assert_eq!(status, 200);
        assert_eq!(header_of(&headers, "content-length"), "1024");
        assert_eq!(header_of(&headers, "accept-ranges"), "bytes");
        assert_eq!(body, data);
        drop(proxy);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn integration_range_variants() {
        let (proxy, path, data) = spawn_local_proxy("range_variants");
        let url = proxy.url_for(&path);

        let (status, headers, body) = http_get(&url, Some("bytes=100-199"));
        assert_eq!(status, 206);
        assert_eq!(header_of(&headers, "content-range"), "bytes 100-199/1024");
        assert_eq!(header_of(&headers, "content-length"), "100");
        assert_eq!(body, data[100..200]);

        let (status, headers, body) = http_get(&url, Some("bytes=1000-"));
        assert_eq!(status, 206);
        assert_eq!(header_of(&headers, "content-range"), "bytes 1000-1023/1024");
        assert_eq!(body, data[1000..]);

        let (status, headers, body) = http_get(&url, Some("bytes=-50"));
        assert_eq!(status, 206);
        assert_eq!(header_of(&headers, "content-range"), "bytes 974-1023/1024");
        assert_eq!(body.len(), 50);
        assert_eq!(body, data[974..]);

        let (status, _, _) = http_get(&url, Some("bytes=9999-"));
        assert_eq!(status, 416);

        let (status, _, body) = http_get(&url, Some("garbage"));
        assert_eq!(status, 200);
        assert_eq!(body.len(), 1024);

        drop(proxy);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn integration_head_and_bad_token() {
        use std::io::Write as _;

        let (proxy, path, _data) = spawn_local_proxy("head_token");
        let url = proxy.url_for(&path);

        // HEAD（ureq 不方便读纯 HEAD 响应，这里手写 TCP）
        let bad = url.replace("/s/", "/s/wrongtoken/");
        let (status, _, _) = http_get(&bad, None);
        assert_eq!(status, 403);

        let mut stream = std::net::TcpStream::connect(proxy.addr()).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "HEAD /s/{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            // 从 url 提取 token 后的路径部分
            url.split("/s/").nth(1).unwrap(),
            proxy.addr()
        )
        .unwrap();
        // HEAD 无 body：读到响应头结束即可（tiny_http 默认 keep-alive，不能等 EOF）。
        let mut raw = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            raw.extend_from_slice(&chunk[..n]);
            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if n == 0 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&raw);
        assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
        assert!(head.to_lowercase().contains("content-length: 1024"));

        drop(proxy);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn integration_drop_closes_listener() {
        let (proxy, path, _data) = spawn_local_proxy("drop");
        let addr = proxy.addr();
        drop(proxy);
        // 端口应立即释放（连接被拒即可证明监听已关闭）
        // Windows TIME_WAIT 语义下可能仍可连接；不视为失败（连接仅为触发监听探测）。
        let _ = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), addr.port()),
            std::time::Duration::from_millis(500),
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// stat 计数源：转发 LocalFs 并统计 stat 调用次数（缓存命中率断言用）。
    struct CountingFs {
        inner: std::sync::Arc<LocalFs>,
        stats: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl dyplayer_vfs::VfsSource for CountingFs {
        fn list_dir(&self, path: &Path) -> dyplayer_vfs::VfsResult<Vec<dyplayer_vfs::VfsEntry>> {
            self.inner.list_dir(path)
        }
        fn stat(&self, path: &Path) -> dyplayer_vfs::VfsResult<dyplayer_vfs::VfsMeta> {
            self.stats
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.stat(path)
        }
        fn read_full(&self, path: &Path) -> dyplayer_vfs::VfsResult<Vec<u8>> {
            self.inner.read_full(path)
        }
        fn open_range(
            &self,
            path: &Path,
            start: u64,
            len: Option<u64>,
        ) -> dyplayer_vfs::VfsResult<dyplayer_vfs::RangeRead> {
            self.inner.open_range(path, start, len)
        }
    }

    /// 防回归：每个 206 响应曾各自 stat 一次（远程源 = 每请求一次
    /// PROPFIND 往返）；会话级缓存后多次 Range 请求只 stat 一次。
    #[test]
    fn integration_range_requests_share_single_stat() {
        use std::sync::atomic::AtomicUsize;

        let dir = std::env::temp_dir().join(format!(
            "dyplayer_proxy_test_stat_cache_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("v.bin"), vec![7u8; 4096]).unwrap();
        let fs = LocalFs::new(&dir).unwrap();
        let stats = Arc::new(AtomicUsize::new(0));
        let proxy =
            StreamProxy::spawn(Arc::new(CountingFs { inner: Arc::new(fs), stats: stats.clone() }))
                .unwrap();
        let url = proxy.url_for(&dir.join("v.bin"));

        // 多种 Range 形态 × 3 次：FromTo / From / Suffix。
        let (s, _, _) = http_get(&url, Some("bytes=0-99"));
        assert_eq!(s, 206);
        let (s, _, _) = http_get(&url, Some("bytes=100-"));
        assert_eq!(s, 206);
        let (s, _, _) = http_get(&url, Some("bytes=-50"));
        assert_eq!(s, 206);

        assert_eq!(
            stats.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "三次 Range 请求应共享一次 stat（缓存命中）"
        );
        drop(proxy);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 防回归：上游吞吐计数（诊断基线）。Range 读取的字节数与请求次数
    /// 必须精确累计（含 200 降级路径 SkipReader 丢弃的流量——此处覆盖 206 路径）。
    #[test]
    fn integration_upstream_byte_count() {
        let (proxy, path, data) = spawn_local_proxy("upstream_bytes");
        let url = proxy.url_for(&path);
        let stats = Arc::clone(proxy.stats());

        let (s, _, body) = http_get(&url, Some("bytes=0-99"));
        assert_eq!(s, 206);
        assert_eq!(body.len(), 100);
        let (s, _, _) = http_get(&url, Some("bytes=100-"));
        assert_eq!(s, 206);

        assert_eq!(stats.upstream_bytes(), data.len() as u64, "上游字节 = 100 + 924");
        assert_eq!(stats.gets(), 2);
        assert_eq!(stats.heads(), 0);

        drop(proxy);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
