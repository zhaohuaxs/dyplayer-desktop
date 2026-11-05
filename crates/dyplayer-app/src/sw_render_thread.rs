//! SW 渲染线程：把 `render_sw` + 像素遍历 + `tex.set` 搬离主线程，
//! 治 SW 路径掉帧（render_sw 的 mpv CPU 缩放 5-10ms + 像素遍历 2-4ms
//! 是主线程大头）。主线程 ui() 只画已有纹理，OSC/鼠标永不冻结。
//!
//! 线程模型与 `overlay_window` 一致：`Arc<Mutex<SwRenderBridge>>` 同步 +
//! `stop: AtomicBool` 退出 + `JoinHandle` drop join。
//!
//! wgpu upload（`tex.set` 标记 delta → 主线程 end_pass 消费 write_texture）
//! 仍在主线程（~1-2ms），但 render_sw + 像素遍历已搬走，主线程大头消除。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use dyplayer_mpv::{MpvRenderContext, MPV_RENDER_UPDATE_FRAME};

/// 主线程 ↔ SW 渲染线程桥。
pub struct SwRenderBridge {
    /// 当前前台纹理 id（渲染线程写，主线程读画）。None=未就绪。
    front_tex_id: Option<egui::TextureId>,
    /// SW 渲染尺寸（主线程按 video_rect+ppp 算，渲染线程读重建纹理）。
    sw_size: [i32; 2],
    /// 暂停态（主线程写，渲染线程读决定是否 render）。
    paused: bool,
    /// 强制重建纹理+清屏（VideoReconfig 置位，hwdec 切换尺寸不变时仍重建）。
    force_reconfig: bool,
}

impl SwRenderBridge {
    fn new() -> Self {
        Self {
            front_tex_id: None,
            sw_size: [0, 0],
            paused: false,
            force_reconfig: false,
        }
    }

    /// 主线程：拿前台纹理 id 画。
    pub fn front_tex_id(&self) -> Option<egui::TextureId> {
        self.front_tex_id
    }

    /// 主线程：写 SW 渲染尺寸（每帧 ui() 调）。
    pub fn set_sw_size(&mut self, size: [i32; 2]) {
        self.sw_size = size;
    }

    /// 主线程：写暂停态。
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// 主线程：VideoReconfig 时请求强制重建（hwdec 切换尺寸不变也重建+清屏）。
    pub fn request_reconfig(&mut self) {
        self.force_reconfig = true;
    }
}

pub struct SwRenderThread {
    bridge: Arc<Mutex<SwRenderBridge>>,
    render_calls: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    /// 渲染线程退出信号（Drop 有界等待用，见 `Drop`）。
    exit_rx: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl SwRenderThread {
    /// 启动 SW 渲染线程。`render`/`ctx` 均 Send+Sync，可跨线程持有。
    pub fn spawn(render: Arc<MpvRenderContext>, ctx: egui::Context) -> Self {
        let bridge = Arc::new(Mutex::new(SwRenderBridge::new()));
        let render_calls = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (exit_tx, exit_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("dyplayer-sw-render".into())
            .spawn({
                let bridge = Arc::clone(&bridge);
                let render_calls = Arc::clone(&render_calls);
                let stop = Arc::clone(&stop);
                move || {
                    sw_render_main(render, ctx, bridge, render_calls, stop);
                    let _ = exit_tx.send(());
                }
            })
            .ok();
        if thread.is_none() {
            tracing::error!("SW 渲染线程 spawn 失败（SW 路径将无画面）");
        }
        Self {
            bridge,
            render_calls,
            stop,
            exit_rx,
            thread,
        }
    }

    pub fn bridge(&self) -> &Arc<Mutex<SwRenderBridge>> {
        &self.bridge
    }

    /// 渲染线程的 SW 渲染计数（主线程读用于诊断浮窗/tick 日志）。
    pub fn render_calls(&self) -> &Arc<AtomicU64> {
        &self.render_calls
    }

    /// 显式停 worker：1s 内退出返回 None；超时返回 `JoinHandle` 供调用方
    /// 在 reaper 中 join（保证 render context free 早于 mpv core 销毁）。
    /// 调用此方法后 `Drop` 不再重复等待。
    pub fn shutdown(mut self) -> Option<JoinHandle<()>> {
        self.stop.store(true, Ordering::Relaxed);
        let thread = self.thread.take()?;
        match self.exit_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => {
                let _ = thread.join();
                None
            }
            Err(_) => {
                tracing::warn!(
                    "SW 渲染线程 1s 内未退出，移交 reaper 保证释放顺序"
                );
                Some(thread)
            }
        }
    }
}

impl Drop for SwRenderThread {
    fn drop(&mut self) {
        // 未显式 `shutdown` 消费时的兜底（如 PlayerSession 异常 drop）。
        if self.thread.is_some() {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                match self.exit_rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(()) => {
                        let _ = t.join();
                    }
                    Err(_) => tracing::warn!(
                        "SW 渲染线程 1s 内未退出，放弃 join（线程持 Arc 克隆自行收尾）"
                    ),
                }
            }
        }
    }
}

/// SW 渲染线程主体：轮询 `request_update` → `render_sw` → 像素遍历 → `tex.set`。
fn sw_render_main(
    render: Arc<MpvRenderContext>,
    ctx: egui::Context,
    bridge: Arc<Mutex<SwRenderBridge>>,
    render_calls: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let mut texture: Option<egui::TextureHandle> = None;
    let mut buffer: Vec<u8> = Vec::new();
    let mut size: [i32; 2] = [0, 0];
    let mut needs_full_render = false;

    while !stop.load(Ordering::Relaxed) {
        let (sw_size, paused, force_reconfig) = {
            let mut b = bridge.lock().unwrap_or_else(|e| {
                tracing::error!(target: "sw-render", "bridge poisoned, recovering");
                e.into_inner()
            });
            let f = b.force_reconfig;
            b.force_reconfig = false;
            (b.sw_size, b.paused, f)
        };

        // ensure：尺寸变化或 force_reconfig（hwdec 切换尺寸不变）时重建纹理与 buffer。
        if sw_size[0] > 0 && sw_size[1] > 0 && (sw_size != size || force_reconfig) {
            size = sw_size;
            let (w, h) = (size[0] as usize, size[1] as usize);
            let image = egui::ColorImage {
                size: [w, h],
                source_size: egui::Vec2::new(w as f32, h as f32),
                pixels: vec![egui::Color32::BLACK; w * h],
            };
            match &mut texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => {
                    texture =
                        Some(ctx.load_texture("sw-render-thread", image, egui::TextureOptions::LINEAR));
                }
            }
            buffer = vec![0u8; w * h * 4];
            if let (Some(t), Ok(mut b)) = (&texture, bridge.lock()) {
                b.front_tex_id = Some(t.id());
            }
            // 重建后强制下一轮真渲染，并通知主线程画新纹理。
            needs_full_render = true;
            ctx.request_repaint();
        }

        if texture.is_none() || size[0] <= 0 {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        // render：始终非阻塞（block=false）让 mpv flip_page 推进。
        // paused 软门控：暂停时仍消费 present/redraw 帧（seek/章节跳转），
        // 仅降频（sleep 10ms）；不硬封死避免暂停改尺寸黑屏。
        let next_sleep_us: i64 = if paused { 10_000 } else { 4_000 };
        let has_frame = (render.request_update() & MPV_RENDER_UPDATE_FRAME) != 0;

        if has_frame || needs_full_render {
            needs_full_render = false;
            // 未启 ADVANCED_CONTROL，NEXT_FRAME_INFO 不可用，无法识别
            // display-sync 重复帧，每帧都做一次 SW 渲染 + 上传。
            render_calls.fetch_add(1, Ordering::Relaxed);
            let result = unsafe { render.render_sw(size[0], size[1], &mut buffer, false) };
            let render_ok = result.is_ok();
            if render_ok {
                // upload：rgb0 直传——先在 buffer 上填 alpha=0xFF（字节遍历，
                // 编译器可向量化连续 4 字节步长写入），再 bulk copy 到 pixels
                // （memcpy 可向量化，省 Color32::from_rgb 逐像素 3 次读 + 函数
                // 调用）。tex.set 标记 delta，主线程 end_pass 消费做 wgpu
                // write_texture。buffer 跨帧复用（仅尺寸变化时重分配）。
                if let Some(tex) = &mut texture {
                    for px in buffer.as_chunks_mut::<4>().0.iter_mut() {
                        px[3] = 0xFF;
                    }
                    let n = (size[0] as usize) * (size[1] as usize);
                    let mut pixels: Vec<egui::Color32> = Vec::with_capacity(n);
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            buffer.as_ptr(),
                            pixels.as_mut_ptr() as *mut u8,
                            n * 4,
                        );
                        pixels.set_len(n);
                    }
                    let image = egui::ColorImage {
                        size: [size[0] as usize, size[1] as usize],
                        source_size: egui::Vec2::new(size[0] as f32, size[1] as f32),
                        pixels,
                    };
                    tex.set(image, egui::TextureOptions::LINEAR);
                }
            } else if let Err(e) = result {
                tracing::error!("sw render thread: mpv render failed: {e}");
            }
            // R03：呈现点显式上报。两条路径都未启 ADVANCED_CONTROL
            // （create_sw / create），mpv 不等 report_swap 闭环，此调用
            // 为空操作；保留以对齐 render API 的呈现语义。
            if render_ok {
                unsafe { render.report_swap() };
            }
            ctx.request_repaint();
        }

        // 轮询节拍：未启 ADVANCED_CONTROL 时 NEXT_FRAME_INFO/target_time
        // 不可用（get_info 失败），按固定节拍轮询；pacing 由 mpv 内部按
        // render 调用节奏估算。不用 mpv_get_time_us——它拿 mpv_handle，
        // 不属于 render.h 允许的 render 线程调用集合。
        thread::sleep(Duration::from_micros(next_sleep_us as u64));
    }
}