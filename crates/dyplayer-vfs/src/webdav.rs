//! WebDAV 源（M3）：ureq 3 同步客户端 + quick-xml 解析 multistatus。
//!
//! 路径模型：VFS path = 相对 base_url 的绝对路径（`/dir/file.mp4`，`/` 分隔）。
//! 认证：Bearer token 优先，其次 Basic；`accept_invalid_certs` 支持自签名 NAS。
//! Range：206 直通；服务器忽略 Range 返回 200 时丢弃前缀降级。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};

use crate::source::VfsSource;
use crate::types::{RangeRead, VfsEntry, VfsError, VfsMeta, VfsResult};

/// 原始 HTTP 响应三元组：状态码 / 头列表 / 响应体 reader。
type HttpReply = (u16, Vec<(String, String)>, Box<dyn Read + Send>);

/// URL 路径段中需要编码的字符（保留 `/`）。
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
    .add(b'\\')
    .add(b'^')
    .add(b'|');

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:getcontentlength/>
    <d:getlastmodified/>
  </d:prop>
</d:propfind>"#;

/// 建立连接（含 TCP/TLS 握手）的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 等待响应头的超时（收到头后失效，适用于所有请求含视频流）。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// 元数据/小文件响应体的总读取预算；视频流不设（见 `build_agent`）。
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// 构建 WebDAV 请求的 ureq Agent。
///
/// `body_timeout=true` 用于元数据/小文件（PROPFIND、弹幕/字幕 GET）：响应体必然
/// 很小，读不完即视为异常，加预算防止服务器 accept 后不回数据时无限挂起。
///
/// `body_timeout=false` 用于视频流 Range GET：body 由代理逐步转发给 mpv，读取
/// 时长取决于播放时长与消费方速度，设总预算会误伤（正常播放被判超时）。
fn build_agent(accept_invalid_certs: bool, body_timeout: bool) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .user_agent("DyPlayer/0.1")
        // WebDAV 的 PROPFIND 等扩展方法不在 HTTP/1.1 标准方法白名单内，
        // ureq 3 默认拒绝（"PROPFIND not valid for HTTP version HTTP/1.1"）。
        .allow_non_standard_methods(true)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT));
    if body_timeout {
        builder = builder.timeout_recv_body(Some(BODY_TIMEOUT));
    }
    if accept_invalid_certs {
        builder = builder.tls_config(
            ureq::tls::TlsConfig::builder()
                .disable_verification(true)
                .build(),
        );
    }
    ureq::Agent::new_with_config(builder.build())
}

#[derive(Debug, Clone, Default)]
pub struct WebDavConfig {
    /// 服务器基地址，如 `https://dav.example.com/dav`（末尾 `/` 会被去除）。
    pub base_url: String,
    pub username: String,
    pub password: String,
    /// Bearer token（设置后优先于 Basic 认证）。
    pub bearer: Option<String>,
    /// 跳过 TLS 证书校验（自签名 NAS 场景）。
    pub accept_invalid_certs: bool,
}

pub struct WebDavFs {
    config: WebDavConfig,
    /// 元数据/小文件请求（PROPFIND、弹幕/字幕全量 GET）：body 读取带预算超时。
    agent: ureq::Agent,
    /// 视频流 Range GET：body 为流式（消费方逐步读），不设 body 预算超时，
    /// 仅保留连接与响应头超时，避免误伤长时间流式播放。
    stream_agent: ureq::Agent,
    /// "服务器忽略 Range 返回 200"降级路径的 warn 只打一次（每实例），
    /// 防止 mpv 密集 Range 请求刷屏日志。
    range_ignored_count: AtomicU64,
}

#[derive(Debug)]
pub(crate) struct PropEntry {
    pub(crate) vpath: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) displayname: Option<String>,
}

impl WebDavFs {
    pub fn new(config: WebDavConfig) -> Self {
        let mut config = config;
        while config.base_url.ends_with('/') {
            config.base_url.pop();
        }
        Self {
            agent: build_agent(config.accept_invalid_certs, true),
            stream_agent: build_agent(config.accept_invalid_certs, false),
            range_ignored_count: AtomicU64::new(0),
            config,
        }
    }

    pub fn config(&self) -> &WebDavConfig {
        &self.config
    }

    fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// base_url 的 URL 路径前缀（如 `/dav`），用于把 href 还原为 VFS path。
    fn base_path(&self) -> &str {
        let url = &self.config.base_url;
        match url.find("://") {
            Some(i) => {
                let rest = &url[i + 3..];
                match rest.find('/') {
                    Some(j) => &rest[j..],
                    None => "",
                }
            }
            None => url,
        }
    }

    fn auth_header(&self) -> Option<String> {
        if let Some(token) = &self.config.bearer {
            return Some(format!("Bearer {token}"));
        }
        if self.config.username.is_empty() && self.config.password.is_empty() {
            return None;
        }
        let raw = format!("{}:{}", self.config.username, self.config.password);
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
        Some(format!("Basic {encoded}"))
    }

    /// 发送任意方法的请求（PROPFIND/GET），返回 (status, headers, reader)。
    fn send(
        &self,
        agent: &ureq::Agent,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> VfsResult<HttpReply> {
        let mut builder = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in headers {
            builder = builder.header(*k, v.as_str());
        }
        let req = builder
            .body(body.to_string())
            .map_err(|e| VfsError::Network(format!("构造请求失败: {e}")))?;
        let resp = agent.run(req).map_err(map_ureq_error)?;
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
        Ok((status, headers, reader))
    }

    fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn propfind(&self, path: &str, depth: &str) -> VfsResult<Vec<PropEntry>> {
        let url = build_url(self.base_url(), &encode_vpath(path));
        let mut headers = vec![("Depth", depth.to_string())];
        if let Some(auth) = self.auth_header() {
            headers.push(("Authorization", auth));
        }
        // ureq 默认把 4xx/5xx 转错误；PROPFIND 的 207 是正常状态码。
        let (status, _, reader) = self.send(&self.agent, "PROPFIND", &url, &headers, PROPFIND_BODY)?;
        let mut xml = String::new();
        reader
            .take(8 * 1024 * 1024)
            .read_to_string(&mut xml)
            .map_err(|e| VfsError::Network(e.to_string()))?;
        if status != 207 && status != 200 {
            return Err(VfsError::Network(format!("PROPFIND 返回状态码 {status}")));
        }
        parse_multistatus(&xml, self.base_path())
    }

    fn prop_entries(&self, path: &Path, depth: &str) -> VfsResult<Vec<PropEntry>> {
        let vpath = path_to_vpath(path);
        self.propfind(&vpath, depth)
    }

    fn get(&self, path: &Path, range: Option<&str>) -> VfsResult<HttpReply> {
        let vpath = path_to_vpath(path);
        let url = build_url(self.base_url(), &encode_vpath(&vpath));
        let mut headers = Vec::new();
        if let Some(auth) = self.auth_header() {
            headers.push(("Authorization", auth));
        }
        if let Some(r) = range {
            headers.push(("Range", r.to_string()));
        }
        // 带宽预算的 agent 选择：Range 请求（视频流）走 stream_agent，避免 body
        // 总预算在长时间流式播放中被误判；其余（弹幕/字幕全量读）走 meta agent。
        let agent = if range.is_some() {
            &self.stream_agent
        } else {
            &self.agent
        };
        // ureq 默认 4xx/5xx -> Error::StatusCode；404 等映射到 VfsError，
        // 因此这里能拿到的 status 只可能是 2xx/3xx。
        self.send(agent, "GET", &url, &headers, "")
    }
}

impl VfsSource for WebDavFs {
    fn list_dir(&self, path: &Path) -> VfsResult<Vec<VfsEntry>> {
        let dir = path_to_vpath(path);
        let props = self.prop_entries(path, "1")?;
        let mut entries = Vec::new();
        for prop in props {
            if prop.vpath == dir || prop.vpath.is_empty() {
                continue;
            }
            let name = prop
                .displayname
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    prop.vpath
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                });
            if name.is_empty() {
                continue;
            }
            let child_path = if prop.vpath.starts_with('/') {
                PathBuf::from(&prop.vpath)
            } else {
                PathBuf::from(format!("/{}", prop.vpath))
            };
            entries.push(VfsEntry {
                name,
                path: child_path,
                is_dir: prop.is_dir,
                size: prop.size,
                modified: prop.modified,
            });
        }
        Ok(entries)
    }

    fn stat(&self, path: &Path) -> VfsResult<VfsMeta> {
        let props = self.prop_entries(path, "0")?;
        let dir = path_to_vpath(path);
        let self_prop = props
            .iter()
            .find(|p| p.vpath == dir)
            .ok_or_else(|| VfsError::NotFound(dir.clone()))?;
        Ok(VfsMeta {
            size: self_prop.size,
            modified: self_prop.modified,
        })
    }

    fn read_full(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let (status, _, mut reader) = self.get(path, None)?;
        if status != 200 {
            return Err(VfsError::Network(format!("GET 返回状态码 {status}")));
        }
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| VfsError::Network(e.to_string()))?;
        Ok(buf)
    }

    fn open_range(&self, path: &Path, start: u64, len: Option<u64>) -> VfsResult<RangeRead> {
        let header = build_range_header(start, len);
        let (status, headers, reader) = self.get(path, Some(&header))?;
        match status {
            206 => {
                let content_range = Self::header_of(&headers, "Content-Range")
                    .and_then(parse_content_range);
                match content_range {
                    Some((rs, re, _total)) => {
                        if rs != start {
                            return Err(VfsError::Network(format!(
                                "服务器返回了错误的范围: 请求从 {start} 开始，得到 {rs}"
                            )));
                        }
                        Ok(RangeRead {
                            reader,
                            length: re - rs + 1,
                        })
                    }
                    None => {
                        let cl = Self::header_of(&headers, "Content-Length")
                            .and_then(|v| v.parse::<u64>().ok())
                            .ok_or_else(|| {
                                VfsError::Network(
                                    "206 响应缺少 Content-Range/Content-Length".into(),
                                )
                            })?;
                        Ok(RangeRead {
                            reader,
                            length: cl,
                        })
                    }
                }
            }
            200 => {
                // 服务器忽略 Range：全量响应，丢弃 start 前的字节后截断。
                // 每实例按 10 次采样 warn 一次（seek 后段将下载并丢弃大前缀，
                // 持续提示用户是服务器能力问题，但不刷屏）。
                let n = self.range_ignored_count.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 10 == 1 {
                    tracing::warn!(
                        "WebDAV 服务器忽略 Range 请求（返回 200 全量，累计 {n} 次）：\
                         seek 到后段将下载并丢弃前缀数据: {}",
                        path.display()
                    );
                }
                let total = Self::header_of(&headers, "Content-Length")
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| VfsError::Network("200 响应缺少 Content-Length".into()))?;
                if start >= total && total > 0 {
                    return Err(VfsError::InvalidRange { start, total });
                }
                let reader = Box::new(SkipReader {
                    inner: reader,
                    remaining: start,
                    trash: vec![0u8; 8192],
                });
                let length = match len {
                    Some(l) => l.min(total - start),
                    None => total - start,
                };
                Ok(RangeRead { reader, length })
            }
            code => Err(VfsError::Network(format!("GET 意外状态码 {code}"))),
        }
    }
}

fn map_ureq_error(err: ureq::Error) -> VfsError {
    match err {
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => {
            VfsError::AccessDenied(format!("HTTP {}（凭据或权限问题）", err))
        }
        ureq::Error::StatusCode(404) => VfsError::NotFound("文件不存在（HTTP 404）".into()),
        ureq::Error::StatusCode(500) => {
            VfsError::Network("HTTP 500（上游服务器内部错误）".into())
        }
        ureq::Error::StatusCode(502) => {
            VfsError::Network("HTTP 502（网关错误）".into())
        }
        ureq::Error::StatusCode(503) => {
            VfsError::Network("HTTP 503（服务暂不可用，可重试）".into())
        }
        ureq::Error::StatusCode(code) => VfsError::Network(format!("HTTP {code}")),
        ureq::Error::HostNotFound => VfsError::Network("域名解析失败".into()),
        ureq::Error::Io(e) => VfsError::Network(e.to_string()),
        other => VfsError::Network(other.to_string()),
    }
}

/// 丢弃开头 N 字节的 reader（服务器忽略 Range 时的降级路径）。
/// 丢弃缓冲 8KB 一次分配循环复用（旧实现每次 read 都新建 vec——
/// 大偏移 seek 时每 8KB 一分配）。
struct SkipReader {
    inner: Box<dyn Read + Send>,
    remaining: u64,
    trash: Vec<u8>,
}

impl Read for SkipReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.remaining > 0 {
            let skip = self.remaining.min(self.trash.len() as u64) as usize;
            let n = self.inner.read(&mut self.trash[..skip])?;
            if n == 0 {
                self.remaining = 0;
                break;
            }
            self.remaining -= n as u64;
        }
        self.inner.read(buf)
    }
}

// ---------- 纯函数（单测覆盖） ----------

/// VFS path → 字符串路径（统一 `/` 分隔，必定以 `/` 开头）。
pub(crate) fn path_to_vpath(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        s
    } else {
        format!("/{s}")
    }
}

/// 对 VFS 路径做 percent-encoding（保留 `/`，编码非 ASCII 与保留字）。
pub(crate) fn encode_vpath(vpath: &str) -> String {
    utf8_percent_encode(vpath, PATH_SEGMENT_SET).to_string()
}

/// 拼接 base_url 与已编码路径。
pub(crate) fn build_url(base: &str, encoded_path: &str) -> String {
    format!("{base}{encoded_path}")
}

/// Range 头：`bytes=start-(start+len-1)` / `bytes=start-`。
pub(crate) fn build_range_header(start: u64, len: Option<u64>) -> String {
    match len {
        Some(l) => format!("bytes={}-{}", start, start + l.saturating_sub(1)),
        None => format!("bytes={start}-"),
    }
}

/// 解析 `Content-Range: bytes 2-9/10` → `Some((start, end, total))`。
pub(crate) fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    let rest = header.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    let total: u64 = if total.trim() == "*" {
        return None;
    } else {
        total.trim().parse().ok()?
    };
    Some((start, end, total))
}

/// href（可能是编码路径 / 全 URL / 相对 base 的路径）→ VFS path。
pub(crate) fn href_to_vpath(href: &str, base_path: &str) -> String {
    let mut h = href.to_string();
    if let Some(i) = h.find("://") {
        if let Some(j) = h[i + 3..].find('/') {
            h = h[i + 3 + j..].to_string();
        } else {
            h = "/".into();
        }
    }
    if let Some(q) = h.find('?') {
        h.truncate(q);
    }
    let decoded = percent_decode_str(&h).decode_utf8_lossy().into_owned();
    let decoded = decoded.replace('\\', "/");
    let stripped = decoded
        .strip_prefix(base_path)
        .unwrap_or(&decoded)
        .to_string();
    let mut v = if stripped.starts_with('/') {
        stripped
    } else {
        format!("/{stripped}")
    };
    while v.len() > 1 && v.ends_with('/') {
        v.pop();
    }
    v
}

/// 解析 RFC1123 时间（`Wed, 15 Oct 2025 03:20:15 GMT`）。
pub(crate) fn parse_http_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let rest = s.split_once(", ").map(|x| x.1).unwrap_or(s);
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let hms = parts.next()?;
    let (h, m, sec) = {
        let mut it = hms.split(':');
        let h: u64 = it.next()?.parse().ok()?;
        let m: u64 = it.next()?.parse().ok()?;
        let sec: u64 = it.next()?.parse().ok()?;
        (h, m, sec)
    };
    // days from civil（Howard Hinnant 算法）
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (if month > 2 { month - 3 } else { month + 9 }) as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    let secs = days * 86400 + (h * 3600 + m * 60 + sec) as i64;
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
}

/// 解析 PROPFIND multistatus 响应体。
pub(crate) fn parse_multistatus(xml: &str, base_path: &str) -> VfsResult<Vec<PropEntry>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut entries = Vec::new();
    let mut current: Option<PropEntry> = None;
    let mut buf = Vec::new();
    // 当前文本所属元素 local name（href / displayname / getcontentlength / getlastmodified）
    let mut text_target: Option<String> = None;
    // 在当前 response 内是否已见 collection
    let mut in_collection = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(&name);
                match local.as_str() {
                    "response" => {
                        current = Some(PropEntry {
                            vpath: String::new(),
                            is_dir: false,
                            size: 0,
                            modified: None,
                            displayname: None,
                        });
                        in_collection = false;
                    }
                    "collection" => in_collection = true,
                    "href" | "displayname" | "getcontentlength" | "getlastmodified" => {
                        text_target = Some(local.to_string());
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let (Some(target), Some(entry)) = (text_target.as_deref(), current.as_mut()) {
                    let text = t.xml_content().unwrap_or_default();
                    match target {
                        "href" => entry.vpath = href_to_vpath(&text, base_path),
                        "displayname" => entry.displayname = Some(text.into_owned()),
                        "getcontentlength" => {
                            entry.size = text.trim().parse().unwrap_or(0);
                        }
                        "getlastmodified" => {
                            entry.modified = parse_http_date(&text);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(&e.name());
                match local.as_str() {
                    "response" => {
                        if let Some(mut entry) = current.take() {
                            entry.is_dir = in_collection || entry.vpath.is_empty();
                            entries.push(entry);
                        }
                        in_collection = false;
                        text_target = None;
                    }
                    "href" | "displayname" | "getcontentlength" | "getlastmodified" => {
                        text_target = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(VfsError::Network(format!("PROPFIND 响应解析失败: {e}"))),
        }
        buf.clear();
    }
    Ok(entries)
}

fn local_name(qname: &quick_xml::name::QName<'_>) -> String {
    let full = String::from_utf8_lossy(qname.as_ref()).into_owned();
    match full.split_once(':') {
        Some((_, local)) => local.to_string(),
        None => full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpath_encoding_handles_cjk_and_reserved() {
        assert_eq!(encode_vpath("/dir/file.mp4"), "/dir/file.mp4");
        assert_eq!(
            encode_vpath("/电影/ep01.mp4"),
            "/%E7%94%B5%E5%BD%B1/ep01.mp4"
        );
        assert_eq!(encode_vpath("/a b.mp4"), "/a%20b.mp4");
        assert_eq!(encode_vpath("/a#1.mp4"), "/a%231.mp4");
    }

    #[test]
    fn url_building_concatenates() {
        assert_eq!(
            build_url("https://h/dav", "/%E7%94%B5%E5%BD%B1"),
            "https://h/dav/%E7%94%B5%E5%BD%B1"
        );
    }

    #[test]
    fn range_header_variants() {
        assert_eq!(build_range_header(0, None), "bytes=0-");
        assert_eq!(build_range_header(100, None), "bytes=100-");
        assert_eq!(build_range_header(100, Some(500)), "bytes=100-599");
        assert_eq!(build_range_header(0, Some(1)), "bytes=0-0");
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 2-9/10"), Some((2, 9, 10)));
        assert_eq!(parse_content_range("bytes 0-0/100"), Some((0, 0, 100)));
        assert_eq!(parse_content_range("bytes 5-99/*"), None);
        assert_eq!(parse_content_range("garbage"), None);
    }

    #[test]
    fn href_variants_normalize_to_vpath() {
        let base = "/dav";
        assert_eq!(href_to_vpath("/dav/dir/", base), "/dir");
        assert_eq!(href_to_vpath("/dav/dir/file.mp4", base), "/dir/file.mp4");
        assert_eq!(
            href_to_vpath("http://h/dav/dir/file.mp4", base),
            "/dir/file.mp4"
        );
        assert_eq!(
            href_to_vpath("/dav/%E7%94%B5%E5%BD%B1/a.mp4", base),
            "/电影/a.mp4"
        );
        assert_eq!(href_to_vpath("/dav/", base), "/");
        assert_eq!(href_to_vpath("/dav", base), "/");
    }

    #[test]
    fn http_date_parses_known_epoch() {
        let t = parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT").unwrap();
        assert_eq!(t, UNIX_EPOCH);
        let t = parse_http_date("Wed, 15 Oct 2025 03:20:15 GMT").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_760_498_415
        );
        assert!(parse_http_date("nonsense").is_none());
    }

    const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/movies/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>movies</d:displayname>
        <d:resourcetype><d:collection/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/ep01.mp4</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>ep01.mp4</d:displayname>
        <d:resourcetype/>
        <d:getcontentlength>1024</d:getcontentlength>
        <d:getlastmodified>Wed, 15 Oct 2025 03:20:15 GMT</d:getlastmodified>
      </d:prop>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/%E7%94%B5%E5%BD%B1/%E5%BC%B9%E5%B9%95.xml</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getcontentlength>42</d:getcontentlength>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn multistatus_parses_entries() {
        let entries = parse_multistatus(MULTISTATUS, "/dav").unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].vpath, "/movies");
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].displayname.as_deref(), Some("movies"));
        assert_eq!(entries[1].vpath, "/ep01.mp4");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].size, 1024);
        assert!(entries[1].modified.is_some());
        assert_eq!(entries[2].vpath, "/电影/弹幕.xml");
        assert_eq!(entries[2].size, 42);
        assert_eq!(entries[2].displayname, None);
    }

    #[test]
    fn multistatus_without_prefix_namespace() {
        let xml = r#"<multistatus xmlns="DAV:">
  <response>
    <href>/a.mkv</href>
    <propstat><prop><getcontentlength>7</getcontentlength></prop></propstat>
  </response>
</multistatus>"#;
        let entries = parse_multistatus(xml, "").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].vpath, "/a.mkv");
        assert_eq!(entries[0].size, 7);
    }

    #[test]
    fn path_to_vpath_normalizes() {
        assert_eq!(path_to_vpath(Path::new("/a/b")), "/a/b");
        assert_eq!(path_to_vpath(Path::new("a/b")), "/a/b");
        assert_eq!(path_to_vpath(Path::new("/")), "/");
    }

    #[test]
    fn base_path_extraction() {
        let fs = WebDavFs::new(WebDavConfig {
            base_url: "https://dav.example.com/dav/".into(),
            ..Default::default()
        });
        assert_eq!(fs.base_url(), "https://dav.example.com/dav");
        assert_eq!(fs.base_path(), "/dav");
        let fs2 = WebDavFs::new(WebDavConfig {
            base_url: "https://host.example.com".into(),
            ..Default::default()
        });
        assert_eq!(fs2.base_path(), "");

        let fs3 = WebDavFs::new(WebDavConfig {
            base_url: "http://127.0.0.1:5244/d".into(),
            ..Default::default()
        });
        assert_eq!(fs3.base_path(), "/d");
    }

    #[test]
    fn auth_header_prefers_bearer_then_basic() {
        let fs = WebDavFs::new(WebDavConfig {
            base_url: "https://h".into(),
            bearer: Some("tok123".into()),
            username: "u".into(),
            password: "p".into(),
            ..Default::default()
        });
        assert_eq!(fs.auth_header().unwrap(), "Bearer tok123");

        let fs2 = WebDavFs::new(WebDavConfig {
            base_url: "https://h".into(),
            username: "user".into(),
            password: "pass".into(),
            ..Default::default()
        });
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"user:pass")
        );
        assert_eq!(fs2.auth_header().unwrap(), expected);

        let fs3 = WebDavFs::new(WebDavConfig {
            base_url: "https://h".into(),
            ..Default::default()
        });
        assert!(fs3.auth_header().is_none());
    }

    #[test]
    fn skip_reader_drops_prefix() {
        let inner: Box<dyn Read + Send> = Box::new(&b"0123456789"[..]);
        let mut r = SkipReader {
            inner,
            remaining: 4,
            trash: vec![0u8; 8192],
        };
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"456789");
    }

    /// 跨多次 read() 的丢弃（缓冲复用路径回归）：分块读取时 remaining
    /// 状态正确累计，不因复用缓冲引入错误。
    #[test]
    fn skip_reader_drops_prefix_across_chunked_reads() {
        let inner: Box<dyn Read + Send> = Box::new(ChunkedReader {
            data: b"0123456789abcdefghij".to_vec(),
            chunk: 3,
        });
        let mut r = SkipReader {
            inner,
            remaining: 10,
            trash: vec![0u8; 4], // 故意小于 chunk，强制多轮丢弃
        };
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcdefghij");
    }

    /// 每次只吐 chunk 字节的 reader（模拟慢速网络流）。
    struct ChunkedReader {
        data: Vec<u8>,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.chunk.min(buf.len()).min(self.data.len());
            if n == 0 {
                return Ok(0);
            }
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data.drain(..n);
            Ok(n)
        }
    }
}