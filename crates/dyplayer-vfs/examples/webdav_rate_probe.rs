//! WebDAV 限流行为探针（手动诊断工具）。
//!
//! 用法：`cargo run -p dyplayer-vfs --example webdav_rate_probe -- [count] [delay_ms]`
//!
//! 两种模式各发 count 次 PROPFIND Depth:0：
//! A) 复用同一 Agent（连接复用）；B) 每次新建 Agent（模拟 DyPlayer 每次浏览
//! 都 open_vfs 新建 WebDavFs 的真实行为）。打印每次状态码与 Retry-After 头，
//! 用于实锤服务器端限流阈值与恢复窗口。

use std::time::{Duration, Instant};

use base64::Engine as _;

const BASE: &str = "http://192.168.5.5:5244/dav";
const USER: &str = "guest";
const PASS: &str = "guest";
const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;

fn build_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .allow_non_standard_methods(true)
            .build(),
    )
}

fn propfind(agent: &ureq::Agent) -> (u16, Option<String>) {
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}"))
    );
    let req = ureq::http::Request::builder()
        .method("PROPFIND")
        .uri(BASE)
        .header("Depth", "0")
        .header("Authorization", auth)
        .body(BODY.to_string())
        .unwrap();
    match agent.run(req) {
        Ok(resp) => {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let status = resp.status().as_u16();
            let _ = resp.into_body().read_to_vec();
            (status, retry_after)
        }
        Err(ureq::Error::StatusCode(code)) => (code, None),
        Err(e) => (0, Some(e.to_string())),
    }
}

fn round(tag: &str, count: usize, delay_ms: u64, fresh_agent: bool) {
    println!("--- 模式 {tag}: {count} 次, 间隔 {delay_ms}ms, fresh_agent={fresh_agent} ---");
    let agent = build_agent();
    let start = Instant::now();
    let mut first_429: Option<usize> = None;
    let mut ok_before_429 = 0;
    for i in 1..=count {
        if delay_ms > 0 && i > 1 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        let a = if fresh_agent { &build_agent() } else { &agent };
        let (status, extra) = propfind(a);
        let t = start.elapsed().as_millis();
        match status {
            207 => {
                if first_429.is_none() {
                    ok_before_429 += 1;
                }
                println!("  #{i:>2} +{t:>6}ms 207 OK");
            }
            0 => println!("  #{i:>2} +{t:>6}ms 请求异常: {}", extra.unwrap_or_default()),
            code => {
                if first_429.is_none() && code == 429 {
                    first_429 = Some(i);
                }
                println!("  #{i:>2} +{t:>6}ms {code} retry-after={:?}", extra);
            }
        }
    }
    println!(
        "  小结: 429 前成功 {ok_before_429} 次, 首次 429 在第 {first_429:?} 次\n"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("recover") {
        recover_mode();
        return;
    }
    let count: usize = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let delay_ms: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    println!("目标: {BASE} (guest/guest)\n");
    round("A-复用Agent", count, delay_ms, false);
    round("B-每次新建Agent", count, delay_ms, true);
}

/// 恢复等待模式：每 10s 发 1 次直到 207（上限 5 分钟），随后 0 间隔急速连发 10 次测窗口配额。
fn recover_mode() {
    println!("目标: {BASE} (guest/guest) — 恢复等待模式\n");
    let agent = build_agent();
    let start = Instant::now();
    let mut wait_secs = 0u64;
    loop {
        let (status, _) = propfind(&agent);
        if status == 207 {
            println!("+{}ms 恢复（等待 {wait_secs}s 后 207 OK）", start.elapsed().as_millis());
            break;
        }
        if start.elapsed() > Duration::from_secs(300) {
            println!("5 分钟未恢复，放弃");
            return;
        }
        std::thread::sleep(Duration::from_secs(10));
        wait_secs += 10;
    }
    println!("\n--- 恢复后 0 间隔急速连发 10 次 ---");
    for i in 1..=10 {
        let (status, _) = propfind(&agent);
        println!("  #{i:>2} +{}ms {status}", start.elapsed().as_millis());
    }
}