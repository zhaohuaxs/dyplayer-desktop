//! update_callback 真实回调路径回归（P1④ 修复验证）：
//! 创建 render context + 设置回调 + 播放 lavfi 测试源 + 等待回调真实触发。
//!
//! 背景：`Arc<dyn Fn>` 的 `into_raw` 是 fat pointer（data+vtable），
//! 不能直接过 C 的 `void* cb_ctx`——曾导致 trampoline 虚调用跳垃圾
//! vtable，播放即崩（UI 卡死后崩溃）。此测试覆盖该路径。
//! 依赖真实 libmpv-2.dll，无 dll 环境静默跳过。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dyplayer_mpv::{ClientOptions, MpvClient, MpvLibrary, MpvRenderContext};

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
fn update_callback_fires_and_drop_is_clean() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    // ao=null：headless 无音频设备（sw_pacing_verify example 同款）。
    let client = MpvClient::create(
        lib,
        &ClientOptions {
            extra_options: vec![("ao".into(), "null".into())],
            ..ClientOptions::default()
        },
    )
    .expect("create client");

    let mut render =
        unsafe { MpvRenderContext::create_sw(&client) }.expect("create render context");

    // 回调闭包捕获共享计数器（模拟 egui Context::request_repaint 的捕获形态：
    // move 闭包 + 单词大小的被捕获值）。
    let fires = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&fires);
    render.set_update_callback(Box::new(move || {
        captured.fetch_add(1, Ordering::SeqCst);
    }));

    // 播放 2 秒 lavfi 测试源（无文件依赖），mpv 产帧即触发 update callback。
    client
        .command(&["loadfile", "avdevice://lavfi:testsrc2=duration=2:size=320x240:rate=30", "replace"])
        .expect("loadfile");

    // 消费循环（block=true：VO↔render 呈现握手，false 会 hold 住后续帧）。
    let mut buffer = vec![0u8; 320 * 240 * 4];
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seen = 0usize;
    while Instant::now() < deadline {
        let _ = render.request_update();
        let _ = unsafe { render.render_sw(320, 240, &mut buffer, true) };
        seen = fires.load(Ordering::SeqCst);
        if seen >= 5 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen >= 5, "update callback 未持续触发（seen={seen}）");

    // drop 顺序与 PlayerSession 一致：render 先于 client。
    drop(render);
    drop(client);
}

/// PlayerSession 的 Drop 顺序契约回归（事件线程持有 client Arc）：
/// render context 先释放、client 后销毁——事件线程存活时 client 的
/// Arc 计数不为零，terminate_destroy 推迟到线程退出之后，render 的
/// mpv_render_context_free 始终作用于存活 handle。此测试以最小组合体
/// 复现该时序（app crate 的 eframe 依赖不适合在集成测试里起完整会话）。
#[test]
fn drop_with_event_thread_alive_is_clean() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    let client = std::sync::Arc::new(
        MpvClient::create(
            lib,
            &ClientOptions {
                extra_options: vec![("ao".into(), "null".into())],
                ..ClientOptions::default()
            },
        )
        .expect("create client"),
    );

    let (tx, _rx) = std::sync::mpsc::channel();
    let event_thread = client.spawn_event_loop(tx, |_| {});

    let mut render =
        unsafe { MpvRenderContext::create_sw(&client) }.expect("create render context");
    let fires = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&fires);
    render.set_update_callback(Box::new(move || {
        captured.fetch_add(1, Ordering::SeqCst);
    }));
    drop(render); // PlayerSession::drop 的顺序：render 先

    // 事件线程退出条件依赖"事件到达 + send 失败"——空闲 client 不产
    // 事件，须显式 quit 触发 Shutdown 事件让线程退出（PlayerSession
    // 的 on_exit 同款路径）。
    let _ = client.command(&["quit"]);
    event_thread.join().expect("event thread panicked");
    drop(client); // 最后一个引用 → terminate_destroy
}
/// 替换语义：第二次 set 后旧回调不再触发、新回调生效（卸载路径
/// unload_update_callback 的锁内置 None + 计数保护）。
#[test]
fn update_callback_replacement_switches_callbacks() {
    let Some(dll) = vendor_dll() else {
        eprintln!("skip: no libmpv-2.dll available");
        return;
    };
    let lib = MpvLibrary::load(Some(&dll)).expect("load libmpv");
    let client = MpvClient::create(
        lib,
        &ClientOptions {
            extra_options: vec![("ao".into(), "null".into())],
            ..ClientOptions::default()
        },
    )
    .expect("create client");
    let mut render =
        unsafe { MpvRenderContext::create_sw(&client) }.expect("create render context");

    let old_fires = Arc::new(AtomicUsize::new(0));
    let new_fires = Arc::new(AtomicUsize::new(0));
    let old_cap = Arc::clone(&old_fires);
    let new_cap = Arc::clone(&new_fires);
    render.set_update_callback(Box::new(move || {
        old_cap.fetch_add(1, Ordering::SeqCst);
    }));
    render.set_update_callback(Box::new(move || {
        new_cap.fetch_add(1, Ordering::SeqCst);
    }));

    client
        .command(&["loadfile", "avdevice://lavfi:testsrc2=duration=2:size=320x240:rate=30", "replace"])
        .expect("loadfile");

    // 模拟真实播放循环：request_update + render_sw 消费帧（block=true 是
    // VO↔render 呈现握手，false 会 hold 住后续帧、管线只出 1 帧）。
    let mut buffer = vec![0u8; 320 * 240 * 4];
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let _ = render.request_update();
        let _ = unsafe { render.render_sw(320, 240, &mut buffer, true) };
        if new_fires.load(Ordering::SeqCst) >= 5 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        new_fires.load(Ordering::SeqCst) >= 5,
        "替换后的新回调未持续触发 (new={}, old={})",
        new_fires.load(Ordering::SeqCst),
        old_fires.load(Ordering::SeqCst)
    );
    // 卸载语义允许 in-flight 尾巴（set None 生效前 mpv 已进入旧回调，
    // 如 render context 建立后的初始 redraw 通知），但不得再被分发：
    // 以卸载后某刻为基线，稳定期内旧计数不得增长、新计数持续增长。
    let old_base = old_fires.load(Ordering::SeqCst);
    let new_base = new_fires.load(Ordering::SeqCst);
    for _ in 0..30 {
        let _ = render.request_update();
        let _ = unsafe { render.render_sw(320, 240, &mut buffer, true) };
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        old_fires.load(Ordering::SeqCst),
        old_base,
        "卸载后的旧回调不得继续分发"
    );
    assert!(
        new_fires.load(Ordering::SeqCst) > new_base,
        "新回调应在稳定期持续触发"
    );
    drop(render);
    drop(client);
}