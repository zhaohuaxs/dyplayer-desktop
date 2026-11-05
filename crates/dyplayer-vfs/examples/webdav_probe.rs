//! WebDAV 真实服务器探针（手动验证工具，不做 CI 断言）。
//!
//! 用法：`cargo run -p dyplayer-vfs --example webdav_probe -- [base_url] [user] [pass] [vpath]`
//! 缺省目标：AList `http://192.168.5.5:5244/dav`，访客账号 guest/guest，根目录 `/`。
//!
//! 验证链路：PROPFIND Depth:1（Basic 认证 + 207 multistatus 解析 + href 归一化）
//! → 列根目录 → 对第一个文件做 Range 16 字节读取（GET/206 或降级 200）。

use std::io::Read;
use std::path::Path;

use dyplayer_vfs::{VfsSource, WebDavConfig, WebDavFs};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base_url = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://192.168.5.5:5244/dav".to_string());
    let username = args.get(2).cloned().unwrap_or_else(|| "guest".to_string());
    let password = args.get(3).cloned().unwrap_or_else(|| "guest".to_string());
    let vpath = args
        .get(4)
        .cloned()
        .unwrap_or_else(|| "/".to_string());

    let fs = WebDavFs::new(WebDavConfig {
        base_url,
        username,
        password,
        ..Default::default()
    });

    println!("== WebDAV 探针: {} (user: {}) ==", fs.config().base_url, fs.config().username);

    println!("\n[1/2] PROPFIND Depth:1 {vpath}");
    let entries = match fs.list_dir(Path::new(&vpath)) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("FAILED: {e:?}");
            std::process::exit(1);
        }
    };
    if entries.is_empty() {
        println!("  (目录为空)");
    }
    for e in &entries {
        let kind = if e.is_dir { "DIR " } else { "FILE" };
        println!("  {kind} {:>12} B  {}", e.size, e.path.display());
    }
    println!("OK: 共 {} 个条目", entries.len());

    println!("\n[2/2] GET Range 16B");
    match entries.iter().find(|e| !e.is_dir && e.size > 0) {
        Some(file) => {
            println!("  目标: {}", file.path.display());
            match fs.open_range(&file.path, 0, Some(16)) {
                Ok(read) => {
                    let mut buf = Vec::new();
                    read.reader.take(64).read_to_end(&mut buf).unwrap();
                    println!("  长度: {} B, 前 16 字节: {:02X?}", read.length, &buf[..buf.len().min(16)]);
                    println!("OK");
                }
                Err(e) => {
                    eprintln!("FAILED: {e:?}");
                    std::process::exit(1);
                }
            }
        }
        None => println!("  (根目录无文件，跳过)"),
    }

    println!("\n全部验证通过");
}