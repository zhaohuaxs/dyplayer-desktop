//! 运行时属性 API 回归：`set_property_string` 必须走 `mpv_set_property_string`
//! （初始化后修改属性的正确 API；旧实现误用 `mpv_set_option_string`，属
//! mpv 未定义行为）。依赖真实 libmpv-2.dll，无 dll 环境静默跳过。

use dyplayer_mpv::{ClientOptions, MpvClient, MpvLibrary};

fn vendor_dll() -> Option<std::path::PathBuf> {
    for rel in [
        "../../vendor/mpv-dev/libmpv-2.dll",
        "vendor/mpv-dev/libmpv-2.dll",
    ] {
        let p = std::path::Path::new(rel);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

#[test]
fn set_property_string_writes_runtime_property() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    let client = MpvClient::create(lib, &ClientOptions::default()).expect("create client");

    // 运行期写 pause 属性并读回（旧实现走 set_option_string 在此时调用
    // 属未定义行为，可能静默失效）。
    client.set_property_string("pause", "no").expect("set pause=no");
    assert!(!client.get_property_flag("pause").expect("read pause"));
    client.set_property_string("pause", "yes").expect("set pause=yes");
    assert!(client.get_property_flag("pause").expect("read pause"));

    // 未知属性应返回错误而非静默成功。
    assert!(client.set_property_string("definitely-not-a-property", "x").is_err());
}

#[test]
fn set_property_i64_roundtrip_volume() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    let client = MpvClient::create(lib, &ClientOptions::default()).expect("create client");
    client.set_property_i64("volume", 42).expect("set volume");
    assert_eq!(client.get_property_i64("volume").expect("read volume"), 42);
}