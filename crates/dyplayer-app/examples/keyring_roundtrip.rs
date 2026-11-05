//! keyring 写读删往返测试（诊断 Windows 凭据管理器后端是否可用）。
//!
//! 用法：`cargo run -p dyplayer-app --example keyring_roundtrip`

use dyplayer_store::secrets;

fn main() {
    let test_id = 999_001;
    println!("[1] 写入 source:{test_id} = 'secret-pw-123' …");
    match secrets::set_source_credential(test_id, "secret-pw-123") {
        Ok(()) => println!("    写入 OK"),
        Err(e) => {
            println!("    写入 FAILED: {e}");
            std::process::exit(1);
        }
    }
    println!("[2] 读回 …");
    match secrets::get_source_credential(test_id) {
        Ok(Some(pw)) => println!("    读回 OK: {pw:?}"),
        Ok(None) => println!("    读回 None（写读不一致！）"),
        Err(e) => println!("    读取 FAILED: {e}"),
    }
    println!("[3] 删除 …");
    match secrets::delete_source_credential(test_id) {
        Ok(()) => println!("    删除 OK"),
        Err(e) => println!("    删除 FAILED: {e}"),
    }
    println!("[4] 再读（应 None）…");
    println!(
        "    {:?}",
        secrets::get_source_credential(test_id).ok().flatten()
    );
}
