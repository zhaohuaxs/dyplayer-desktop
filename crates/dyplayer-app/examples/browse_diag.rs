//! WebDAV 浏览链路诊断探针（手动工具）。
//!
//! 复现 UI 浏览路径（keyring 凭证 → WebDavFs → list_dir），逐环节打印状态：
//! 1) DB sources 表内容；2) keyring 凭证读取结果；3) list_dir 实测。
//!
//! 用法：`cargo run -p dyplayer-app --example browse_diag -- [source_id]`
//! 缺省诊断所有远程源。

use dyplayer_store::{Store, secrets};
use dyplayer_vfs::{VfsSource, WebDavConfig, WebDavFs};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set") {
        let id: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
            eprintln!("用法: browse_diag set <source_id> <password>");
            std::process::exit(2);
        });
        let Some(pw) = args.get(3) else {
            eprintln!("用法: browse_diag set <source_id> <password>");
            std::process::exit(2);
        };
        match dyplayer_store::secrets::set_source_credential(id, pw) {
            Ok(()) => println!("已写入 source:{id} 凭证（写后读回校验通过）"),
            Err(e) => {
                eprintln!("写入失败: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let filter_id: Option<i64> = args.get(1).and_then(|s| s.parse().ok());
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打开数据库失败: {e}");
            std::process::exit(1);
        }
    };
    let rows: Vec<(i64, String, String, String, String, String, String)> =
        store.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, kind, name, endpoint, username, root, extra_json FROM sources")
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|(id, kind, ..)| kind != "local" && filter_id.map(|f| f == *id).unwrap_or(true))
            .collect()
        });

    if rows.is_empty() {
        println!("没有匹配的远程源");
        return;
    }

    for (id, kind, name, endpoint, username, root, extra) in &rows {
        println!("==================================================");
        println!("源 #{id} [{kind}] {name}");
        println!("  endpoint : {endpoint}");
        println!("  username : {username:?}");
        println!("  root     : {root:?}");
        println!("  extra    : {extra}");

        // 环节 1：keyring 读取（与 browser.rs keyring_password 同路径）
        match secrets::get_source_credential(*id) {
            Ok(Some(pw)) => println!("  keyring  : OK（长度 {}）", pw.len()),
            Ok(None) => println!("  keyring  : None（没有存储凭证！）"),
            Err(e) => println!("  keyring  : 读取失败：{e}"),
        }

        // 环节 2：复现 open_vfs()（keyring 密码 or 空）+ list_dir
        let password = secrets::get_source_credential(*id)
            .ok()
            .flatten()
            .unwrap_or_default();
        if kind == "webdav" {
            let fs = WebDavFs::new(WebDavConfig {
                base_url: endpoint.clone(),
                username: username.clone(),
                password,
                bearer: None,
                accept_invalid_certs: extra.contains("accept_invalid_certs\":true"),
            });
            let root = if root.is_empty() { "/" } else { root.as_str() };
            println!("  list_dir({root}) …");
            match fs.list_dir(Path::new(root)) {
                Ok(entries) => {
                    println!("  结果    : OK，{} 个条目", entries.len());
                    for e in entries.iter().take(5) {
                        println!("    {:?} {}", e.is_dir, e.path.display());
                    }
                }
                Err(e) => println!("  结果    : FAILED {e:?}"),
            }
        } else {
            println!("  （smb 源跳过 list_dir 实测）");
        }
    }
    println!("==================================================");
}
