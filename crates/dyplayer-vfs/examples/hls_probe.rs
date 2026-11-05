//! HLS 分段 URL 探测（手动验证工具，不做 CI 断言）。
//!
//! 用法：`cargo run -p dyplayer-vfs --example hls_probe -- [capture_file] [base_url] [user] [pass]`
//! capture_file 缺省 `%TEMP%/dyplayer_hls_capture.txt`（proxy 嗅探落盘的 manifest 首块）。
//!
//! 从 manifest 取第一段 URI（含 query 签名），对多个候选路径前缀做 GET Range 0-15，
//! 判定网关是否把转码流分段代理到 OSS（决定 HLS 支持能否在客户端侧修复）。

use std::io::Read;

/// 按保留字符集做 percent-encode（其余字符/UTF-8 字节 → %XX）。
fn pct(s: &str, keep: &[char]) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') || keep.contains(&ch) {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let capture = args.get(1).cloned().unwrap_or_else(|| {
        let mut p = std::env::temp_dir();
        p.push("dyplayer_hls_capture.txt");
        p.to_string_lossy().into_owned()
    });
    let base = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "http://118.89.62.59:5050".to_string());
    let user = args.get(3).cloned().unwrap_or_else(|| "123456789".to_string());
    let pass = args.get(4).cloned().unwrap_or_else(|| "123456789".to_string());

    let content = match std::fs::read_to_string(&capture) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 capture 失败 {capture}: {e}");
            std::process::exit(1);
        }
    };
    let manifest_path = content
        .lines()
        .find(|l| l.starts_with("path="))
        .map(|l| l.trim_start_matches("path=").to_string())
        .unwrap_or_default();
    let uri = match content
        .lines()
        .find(|l| l.starts_with("media-") && l.contains(".ts"))
    {
        Some(u) => u.to_string(),
        None => {
            eprintln!("capture 无 media-*.ts 分段行");
            std::process::exit(1);
        }
    };
    let (name, query) = match uri.split_once('?') {
        Some((n, q)) => (n.to_string(), q.to_string()),
        None => (uri.clone(), String::new()),
    };
    let dir = manifest_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();

    println!("== HLS 分段探测 ==");
    println!("manifest: {manifest_path}");
    println!("dir:      {dir}");
    println!("seg:      {name} (query {} 字节)", query.len());

    let auth = {
        use base64::Engine;
        let raw = format!("{user}:{pass}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
        )
    };
    let agent = ureq::Agent::config_builder()
        .user_agent("DyPlayer-probe/0.1")
        .http_status_as_error(false)
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(20)))
        .build();
    let agent = ureq::Agent::new_with_config(agent);

    let q = pct(&query, &['=', '&']);
    let seg = pct(&name, &[]);
    let dir_enc = pct(&dir, &['/']);
    let manifest_name = manifest_path
        .rsplit_once('/')
        .map(|(_, f)| f.to_string())
        .unwrap_or_default();
    let candidates: Vec<(&str, String)> = vec![
        ("A 同目录", format!("{base}{dir_enc}/{seg}?{q}")),
        ("B 网关根", format!("{base}/{seg}?{q}")),
        (
            "C manifest当目录",
            format!("{base}{dir_enc}/{}/{seg}?{q}", pct(&manifest_name, &[])),
        ),
        ("D 同目录无query", format!("{base}{dir_enc}/{seg}")),
    ];

    for (label, url) in candidates {
        let req = ureq::http::Request::builder()
            .method("GET")
            .uri(&url)
            .header("Authorization", auth.as_str())
            .header("Range", "bytes=0-15")
            .body(String::new());
        let req = match req {
            Ok(r) => r,
            Err(e) => {
                println!("[{label}] URL 构造失败: {e}");
                continue;
            }
        };
        match agent.run(req) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let ct = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                let cl = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                let mut buf = Vec::new();
                let _ = resp.into_body().into_reader().take(16).read_to_end(&mut buf);
                println!(
                    "[{label}] status={status} ct={ct} cl={cl} head={:02X?}",
                    &buf[..buf.len().min(16)]
                );
            }
            Err(e) => println!("[{label}] 传输错误: {e}"),
        }
        println!("      url={url}");
    }
    println!("探测完成");
}