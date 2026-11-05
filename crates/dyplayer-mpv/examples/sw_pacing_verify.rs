//! headless 验证（display 配速方案落地前提）：SW render API 下
//! `video-sync=display-resample + display-fps-override=<hz>` 是否让
//! update callback 以屏幕刷新率（含重复帧）触发，而非视频帧率。
//!
//! 两种循环：
//! - `audio`：callback 驱动（模拟 DyPlayer 现状：UI 被 mpv 24Hz 锁定）
//! - `*_ui60`：UI 自主 60Hz 节拍（模拟真机 eframe + winit vsync 的帧节奏）。
//!   vo_libmpv.c 的 flip_page 等 render 取帧、以实际时刻校准 vsync 估计；
//!   无节拍的全速响应会污染该估计（上轮实验：callback 疯转 3730/s、
//!   视频 2s 放完 10s），故 headless 必须补上 vsync 节流这一环。
//!
//! 用法：
//! cargo run -p dyplayer-mpv --example sw_pacing_verify -- [audio|audio_ui60|display60_ui60|display120_ui60]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dyplayer_mpv::{
    ClientOptions, MPV_RENDER_UPDATE_FRAME, MpvClient, MpvLibrary, MpvRenderContext,
};

const W: i32 = 320;
const H: i32 = 240;
const RUN_SECS: u64 = 10;
const UI_TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "audio".into());
    let mut extra: Vec<(String, String)> = Vec::new();
    let ui_driven = match mode.as_str() {
        "audio" => false,
        "audio_ui60" => true,
        "display60_ui60" | "display120_ui60" => {
            let hz = if mode == "display60_ui60" { "60" } else { "120" };
            extra.push(("video-sync".into(), "display-resample".into()));
            extra.push(("display-fps-override".into(), hz.into()));
            true
        }
        other => anyhow::bail!("未知模式 {other}：audio|audio_ui60|display60_ui60|display120_ui60"),
    };
    extra.push(("ao".into(), "null".into()));

    println!("== mode={mode} ui_driven={ui_driven} extra={extra:?} ==");

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let dll = root.join("dyplayer/vendor/mpv-dev/libmpv-2.dll");
    let video = root.join("tmp_verify/test24.mp4");
    anyhow::ensure!(dll.exists(), "dll 不存在: {dll:?}");
    anyhow::ensure!(video.exists(), "测试视频不存在: {video:?}");

    let lib = MpvLibrary::load(Some(&dll))?;
    let options = ClientOptions {
        hwdec: "no".into(),
        extra_options: extra,
    };
    let client = MpvClient::create(lib, &options)?;

    let (tx, rx) = mpsc::channel::<()>();
    let callback_count = std::sync::Arc::new(AtomicU64::new(0));
    let mut render = unsafe { MpvRenderContext::create_sw(&client) }?;
    {
        let counter = std::sync::Arc::clone(&callback_count);
        render.set_update_callback(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(());
        }));
    }

    client.load_file(video.to_string_lossy().as_ref())?;

    let mut buffer = vec![0u8; (W * H * 4) as usize];
    let mut frame_renders: u64 = 0;
    let mut skip_renders: u64 = 0;
    let mut passes: u64 = 0;
    let mut last_sample = Instant::now();
    let start = Instant::now();
    let mut cb_at_sample = 0u64;
    let mut fr_at_sample = 0u64;
    let mut sk_at_sample = 0u64;
    let mut ps_at_sample = 0u64;
    let mut next_tick = Instant::now();

    // 取帧渲染（DyPlayer 真实逻辑）：repeat 重复帧走 SKIP_RENDERING，
    // 否则全量 SW 渲染。
    fn draw(render: &MpvRenderContext, buffer: &mut [u8], stats: &mut (u64, u64)) {
        let repeat = render
            .next_frame_info()
            .is_some_and(|info| info.repeat && !info.redraw);
        let r = if repeat {
            unsafe { render.render_sw_skip(true) }
        } else {
            stats.0 += 1;
            unsafe { render.render_sw(W, H, buffer, true) }
        };
        if repeat && r.is_ok() {
            stats.1 += 1;
        }
    }

    loop {
        if ui_driven {
            // 模拟 eframe：固定 60Hz 帧节拍，每帧先 drain 唤醒、再取帧渲染
            next_tick += UI_TICK;
            passes += 1;
            while rx.try_recv().is_ok() {}
            let flags = render.request_update();
            if flags & MPV_RENDER_UPDATE_FRAME != 0 {
                let mut stats = (frame_renders, skip_renders);
                draw(&render, &mut buffer, &mut stats);
                frame_renders = stats.0;
                skip_renders = stats.1;
            }
            let now = Instant::now();
            if next_tick > now {
                std::thread::sleep(next_tick - now);
            }
        } else if rx.recv_timeout(Duration::from_millis(50)).is_ok() {
            passes += 1;
            let flags = render.request_update();
            if flags & MPV_RENDER_UPDATE_FRAME != 0 {
                let mut stats = (frame_renders, skip_renders);
                draw(&render, &mut buffer, &mut stats);
                frame_renders = stats.0;
                skip_renders = stats.1;
            }
        }
        let now = Instant::now();
        if now.duration_since(last_sample) >= Duration::from_secs(2) {
            let dt = now.duration_since(last_sample).as_secs_f64();
            let cb = callback_count.load(Ordering::Relaxed);
            println!(
                "t={:4.1}s  callback={:6.1}/s  ui_pass={:6.1}/s  render_sw={:6.1}/s  skip(repeat)={:6.1}/s",
                now.duration_since(start).as_secs_f64(),
                (cb - cb_at_sample) as f64 / dt,
                (passes - ps_at_sample) as f64 / dt,
                (frame_renders - fr_at_sample) as f64 / dt,
                (skip_renders - sk_at_sample) as f64 / dt,
            );
            cb_at_sample = cb;
            ps_at_sample = passes;
            fr_at_sample = frame_renders;
            sk_at_sample = skip_renders;
            last_sample = now;
        }
        if start.elapsed() >= Duration::from_secs(RUN_SECS) {
            break;
        }
    }

    println!(
        "== 总计 {RUN_SECS}s: callback={} passes={} render_sw={} skip={} ==",
        callback_count.load(Ordering::Relaxed),
        passes,
        frame_renders,
        skip_renders
    );
    drop(render);
    let _ = client.command(&["quit"]);
    Ok(())
}