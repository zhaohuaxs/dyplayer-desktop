//! node guard + 版本协商运行时回归。依赖真实 libmpv-2.dll，无 dll 环境静默跳过。

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
fn api_version_meets_requirement() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    let (major, minor) = lib.api_version;
    assert!(
        (major, minor) >= (2, 1),
        "libmpv client API too old: got ({major},{minor})",
    );
    // vendor dll 实测为 (2,5)；只断言 major==2 避免绑死 minor
    assert_eq!(major, 2, "expected client API major 2, got {major}");
}

#[test]
fn empty_track_and_chapter_list_no_crash() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    let client = MpvClient::create(lib, &ClientOptions::default()).expect("create client");
    // 未加载任何媒体：track-list / chapter-list 应为空（不 panic、不泄漏）
    let tracks = client.get_track_list().expect("track-list");
    assert!(tracks.is_empty(), "no media loaded => empty track-list");
    let chapters = client.get_chapter_list().expect("chapter-list");
    assert!(chapters.is_empty(), "no media loaded => empty chapter-list");
}