//! WebDAV 回归测试：tiny_http 本地 mock 服务端，覆盖 PROPFIND（WebDAV 扩展方法）全链路。
//!
//! 回归背景：ureq 3 默认拦截非标准 HTTP 方法（报
//! "PROPFIND not valid for HTTP version HTTP/1.1"，ureq 上游 #1034），
//! `WebDavFs::new` 的 Agent 必须开启 `allow_non_standard_methods(true)`。
//! 本测试用真实 HTTP 往返（客户端发请求 → mock 断言方法/头 → 回 207/206）
//! 保证该配置与解析链路不再回归。

use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use dyplayer_vfs::{VfsSource, WebDavConfig, WebDavFs};

const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/movies/</d:href>
    <d:propstat><d:prop><d:displayname>movies</d:displayname>
      <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ep01.mp4</d:href>
    <d:propstat><d:prop><d:displayname>ep01.mp4</d:displayname>
      <d:resourcetype/><d:getcontentlength>1024</d:getcontentlength></d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

/// mock 服务端观测到的一次请求。
#[derive(Debug)]
struct Observed {
    method: String,
    depth: Option<String>,
    range: Option<String>,
}

fn header_of(headers: &[tiny_http::Header], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// 启动 mock WebDAV 服务：PROPFIND → 207 multistatus；GET → 206 分片响应。
fn spawn_mock() -> (String, mpsc::Receiver<Observed>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let method = request.method().as_str().to_string();
            let depth = header_of(request.headers(), "Depth");
            let range = header_of(request.headers(), "Range");
            let _ = tx.send(Observed {
                method: method.clone(),
                depth,
                range,
            });
            // 排空请求体，保证连接可复用（keep-alive）。
            let mut sink = String::new();
            let _ = request.as_reader().read_to_string(&mut sink);
            let response = if method == "PROPFIND" {
                tiny_http::Response::from_string(MULTISTATUS).with_status_code(207)
            } else {
                tiny_http::Response::from_string("0123456789")
                    .with_status_code(206)
                    .with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Range"[..],
                            &b"bytes 0-9/1024"[..],
                        )
                        .unwrap(),
                    )
            };
            let _ = request.respond(response);
        }
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

#[test]
fn propfind_and_range_over_mock_server() {
    let (base, rx) = spawn_mock();
    let fs = WebDavFs::new(WebDavConfig {
        base_url: base,
        username: "user".into(),
        password: "pass".into(),
        ..Default::default()
    });

    let entries = fs.list_dir(Path::new("/")).unwrap();
    assert_eq!(entries.len(), 2);
    let movies = entries.iter().find(|e| e.name == "movies").unwrap();
    assert!(movies.is_dir);
    assert_eq!(movies.path, Path::new("/movies"));
    let ep = entries.iter().find(|e| e.name == "ep01.mp4").unwrap();
    assert!(!ep.is_dir);
    assert_eq!(ep.size, 1024);

    let mut read = fs.open_range(Path::new("/ep01.mp4"), 0, Some(10)).unwrap();
    assert_eq!(read.length, 10);
    let mut buf = Vec::new();
    read.reader.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"0123456789");

    let propfind = rx.recv().unwrap();
    assert_eq!(propfind.method, "PROPFIND");
    assert_eq!(propfind.depth.as_deref(), Some("1"));
    let get = rx.recv().unwrap();
    assert_eq!(get.method, "GET");
    assert_eq!(get.range.as_deref(), Some("bytes=0-9"));
}