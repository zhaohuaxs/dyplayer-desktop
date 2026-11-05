use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;


use std::time::{Duration, Instant};

use crate::danmaku_ui::DanmakuOverlay;
use anyhow::Context as _;
use dyplayer_mpv::{
    ClientOptions, EngineEvent, MediaClock, MpvClient, MpvLibrary, MpvRenderContext, ObsFormat,
    PropertyChange,
};
#[cfg(not(feature = "backend-wgpu"))]
use dyplayer_mpv::MPV_RENDER_UPDATE_FRAME;
use dyplayer_store::{PlayerConfig, Store};
use dyplayer_vfs::{SubtitleCandidate, VfsSource};

use crate::sw_render_thread::SwRenderThread;

use crate::browser::MediaSource;

use dyplayer_proxy::StreamProxy;

#[cfg(not(feature = "backend-wgpu"))]
use eframe::glow::{self, HasContext};

/// 播放页自绘控制层（ModernZ 风格 OSC；子模块可访问本模块私有状态）。
#[path = "osc.rs"]
pub(crate) mod osc;

const PROGRESS_SAVE_INTERVAL: Duration = Duration::from_secs(5);
const RESUME_MIN_POSITION: f64 = 10.0;

/// ↑/↓ 键盘调节音量的单次步长。
const VOLUME_KEY_STEP: i64 = 5;
/// 解码统计（编码/软硬解/丢帧）刷新间隔。
const STATS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// 设置/弹幕设置面板与按钮栏顶部的间隙。
const PANEL_GAP_ABOVE_BAR: f32 = 4.0;

/// 播放页三面板（队列/弹幕设置/设置），任一时刻至多显示一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayerPanel {
    Queue,
    DanmakuSettings,
    Settings,
}


/// 面板互斥开关：目标面板已开则全关（toggle），未开则只开它。
/// 返回 (queue, danmaku_settings, settings) 三个开关的新状态。
fn exclusive_panel_state(target: PlayerPanel, target_is_open: bool) -> (bool, bool, bool) {
    let open = !target_is_open;
    (
        target == PlayerPanel::Queue && open,
        target == PlayerPanel::DanmakuSettings && open,
        target == PlayerPanel::Settings && open,
    )
}

/// 冻结取证探针：frame_seq 由 App::ui 每帧递增，phase 标记当前执行阶段。
/// watchdog 线程据此检测 ui 线程停滞并在触发时记录实时 phase。
#[derive(Default)]
pub struct FreezeProbe {
    pub frame_seq: std::sync::atomic::AtomicU64,
    pub phase: std::sync::atomic::AtomicU8,
}

impl FreezeProbe {
    pub fn set_phase(&self, phase: u8) {
        self.phase
            .store(phase, std::sync::atomic::Ordering::Relaxed);
    }
}

pub const PH_IDLE: u8 = 0;
pub const PH_POLL: u8 = 1;
pub const PH_FILE_LOADED: u8 = 2;
pub const PH_RECONFIG: u8 = 3;
pub const PH_TIME_POS: u8 = 4;
pub const PH_PROP_FORMAT: u8 = 5;
pub const PH_PROP_HWDEC: u8 = 6;
pub const PH_PROP_DROPS: u8 = 7;
pub const PH_SAVE_PROGRESS: u8 = 8;
pub const PH_UI_START: u8 = 30;
pub const PH_UI_TRANSPORT: u8 = 31;
pub const PH_UI_CENTRAL: u8 = 32;
pub const PH_UI_DANMAKU: u8 = 33;
pub const PH_UI_HUD: u8 = 34;
pub const PH_UI_END: u8 = 39;

pub fn phase_name(phase: u8) -> &'static str {
    match phase {
        0 => "idle",
        PH_POLL => "poll_events",
        PH_FILE_LOADED => "ev:file-loaded",
        PH_RECONFIG => "ev:video-reconfig",
        PH_TIME_POS => "prop:time-pos",
        PH_PROP_FORMAT => "prop:video-format",
        PH_PROP_HWDEC => "prop:hwdec-current",
        PH_PROP_DROPS => "prop:vo-drop-frame-count",
        PH_SAVE_PROGRESS => "save_progress",
        PH_UI_START => "ui:start",
        PH_UI_TRANSPORT => "ui:osc",
        PH_UI_CENTRAL => "ui:central(mpv)",
        PH_UI_DANMAKU => "ui:danmaku",
        PH_UI_HUD => "ui:osc-info",
        PH_UI_END => "ui:end",
        _ => "unknown",
    }
}

#[derive(Debug, Clone)]
pub struct MediaItem {
    pub name: String,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub size: u64,
    pub subs: Vec<SubtitleCandidate>,
}

#[derive(Debug, Clone)]
pub struct PendingPlay {
    pub source_id: i64,
    pub items: Vec<MediaItem>,
    pub start_index: usize,
    /// 远程源（WebDAV/SMB）时携带源定义：PlayerSession 据此构建本地流代理。
    pub remote: Option<MediaSource>,
    /// 串流播放自定义 HTTP 头（多行 `Key: Value`），None=无自定义头。
    pub http_headers: Option<String>,
}

pub struct PlayerSession {
    client: Option<std::sync::Arc<MpvClient>>,
    render: Option<std::sync::Arc<MpvRenderContext>>,
    /// SW 渲染线程（backend-wgpu + !gpu_wid）：render_sw + upload 搬离主线程。
    sw_thread: Option<SwRenderThread>,
    clock: std::sync::Arc<MediaClock>,
    events: mpsc::Receiver<EngineEvent>,
    event_thread: Option<std::thread::JoinHandle<()>>,

    #[allow(dead_code)]
    video: VideoSurface,
    source_id: i64,
    /// 串流播放自定义 HTTP 头（多行 `Key: Value`），None=无自定义头。
    /// load_current 据此设置 mpv http-header-fields。
    http_headers: Option<String>,
    items: Vec<MediaItem>,
    index: usize,
    video_size: Option<[i32; 2]>,
    paused: bool,
    duration: f64,
    /// 当前媒体章节列表（FileLoaded 后从 mpv 拉取；进度条圆点）。
    chapters: Vec<dyplayer_mpv::Chapter>,
    /// 已缓冲前沿时长（秒，demuxer-cache-time；进度条缓冲条）。
    cache_time: f64,
    volume: i64,
    status: String,
    frame_counter: u64,
    last_fps_sample: Instant,
    fps: f64,
    /// 当前视频编码（mpv `video-format`，如 "h264"）。
    codec: String,
    /// 当前解码方式（mpv `hwdec-current`，"no" = 软解）。
    hwdec: String,
    /// VO 累计丢帧数（mpv `vo-drop-frame-count`）。
    vo_drops: i64,
    last_stats_sample: Instant,
    /// 视频帧率（mpv container-fps；信息块显示的媒体属性）。
    /// 注意与 self.fps（egui UI 重绘率，诊断用）区分。
    video_fps: f64,
    /// 弹幕渲染帧计数（本帧弹幕层有可见内容时 +1；流畅度评估）。
    dm_frame_counter: u64,
    /// 弹幕渲染帧率（有弹幕内容提交的 UI 帧数 / 秒；关闭或无可见弹幕时为 0）。
    dm_fps: f64,
    /// 心跳日志：ui() 每秒一条，用于诊断帧循环冻结（全屏等场景）。
    ui_tick: u64,
    last_tick_log: Instant,
    /// 诊断差分基线：上次 tick 快照的上游累计字节 / GET 数
    /// （与 proxy 的 ProxyStats 差分得速率与重连频次）。
    diag_last_upstream: u64,
    diag_last_gets: u64,
    diag_last_rate_tick: Instant,
    /// Reconfig 诊断 dump 限流（同文件 reconfig 可能连发，防刷屏）。
    last_reconfig_diag: Instant,
    /// 诊断：mpv PaintCallback 实际执行次数（跨线程闭包计数）。
    render_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// 冻结取证探针（主循环注入；None 时探针静默）。
    pub probe: Option<Arc<FreezeProbe>>,
    /// OSC 交互瞬态（显隐状态机 / seek 预览 / 轨道菜单开关 / 弹幕按钮
    /// 锚点）：SW 路径在本会话内、GPU 路径在覆盖线程各持一份实例。
    pub(super) osc_ui: crate::osc_state::OscUiState,
    /// 当前播放速度（mpv speed 镜像；OSC 倍速按钮 / MediaClock 同步）。
    speed: f64,
    /// 静音状态（mpv mute 镜像；OSC 音量按钮）。
    muted: bool,
    /// 单集循环（mpv loop-file 镜像；OSC 循环按钮）。
    loop_file: bool,
    /// 音轨列表（FileLoaded 后从 mpv track-list 拉取）。
    audio_tracks: Vec<dyplayer_mpv::Track>,
    /// 字幕轨列表（FileLoaded 后从 mpv track-list 拉取）。
    sub_tracks: Vec<dyplayer_mpv::Track>,
    /// OSC 主题索引（osc::THEMES；设置页切换）。
    osc_theme_idx: usize,
    /// OSC 布局（Default 全功能 / Mini 迷你；设置页切换）。
    osc_layout: crate::osc_state::OscLayout,
    /// OSC 章节标记样式（设置页切换）。
    osc_nibble: crate::osc_state::OscNibble,
    /// OSC hover 特效：光晕（默认关）。
    osc_fx_glow: bool,
    /// OSC hover 特效：放大。
    osc_fx_size: bool,
    /// OSC hover 特效：变色。
    osc_fx_color: bool,
    /// 全屏（fs_patch 屏幕全屏）状态：ESC 或 OSC 全屏按钮退出。
    fullscreen: bool,
    /// 全屏几何诊断日志（每轮全屏只打一次）。
    fs_geom_logged: bool,
    /// 自管全屏进入请求待处理（ui() 顶部拿到 hwnd 后执行）。
    fs_pending: bool,
    /// 自管全屏状态（进入时保存，退出时恢复）。
    #[cfg_attr(not(feature = "backend-wgpu"), allow(dead_code))]
    fs_guard: Option<crate::fs_patch::FullscreenGuard>,
    show_queue: bool,
    /// 播放器设置面板（HUD 开关等）是否打开。
    settings_open: bool,

    danmaku: DanmakuOverlay,
    /// 拖入的 XML 弹幕待应用路径（FileLoaded 同名自动加载之后应用，覆盖同名轨）。
    pending_danmaku: Option<PathBuf>,
    /// 启动视图偏好镜像（设置窗可改，写回 config.toml 的 [ui] 段）。
    ui_startup_view: dyplayer_store::StartupView,
    /// 全局明暗主题镜像（设置窗可改并写回 config；main.rs 每帧回读应用 visuals）。
    ui_theme: dyplayer_store::ThemeMode,
    /// 远程源定义（None = 本地播放）。
    remote: Option<MediaSource>,
    /// 远程源构建的 VFS（字幕/弹幕下载用）。
    vfs: Option<std::sync::Arc<dyn VfsSource>>,
    /// 本地 HTTP Range 流代理（远程源播放用）。
    proxy: Option<dyplayer_proxy::StreamProxy>,
    /// egui 上下文（远程弹幕/字幕预取线程完成后 request_repaint 唤醒 UI）。
    egui_ctx: egui::Context,
    /// 远程弹幕/字幕后台预取任务：发送后置位，UI 帧轮询取结果（见
    /// `poll_remote_prefetch`）；避免 UI 线程做同步网络 IO。
    remote_prefetch: Option<mpsc::Receiver<RemotePrefetch>>,
    /// 本次加载的预取弹幕是否已被显式覆盖（拖入 XML 等）：覆盖后到达的先取
    /// 结果不再应用弹幕，只应用字幕，保持"显式选择优先"的既有语义。
    prefetch_danmaku_overridden: bool,

    pub requested_exit: bool,
    store: std::sync::Arc<Store>,
    config: PlayerConfig,
    /// 引擎配置镜像（设置面板改解码方式后写回 config.toml [engine] 段）。
    engine: dyplayer_store::EngineConfig,
    /// 解码方式变更已请求：由 App 帧循环按新 engine 配置重建会话。
    hwdec_rebuild: bool,
    /// GPU wid 渲染路径生效（本会话）：mpv 自管 D3D11 呈现，egui 侧无
    /// SW 纹理渲染（render=None），由宿主子窗口承载视频（M1）。
    gpu_wid: bool,
    /// mpv wid 宿主子窗口（GPU 路径；构造时隐藏创建，ui() 首帧挂载）。
    host: Option<crate::wid_host::VideoHost>,
    /// 宿主窗口是否已重父进 eframe 主窗口（每会话一次）。
    host_attached: bool,
    /// eframe 主窗口 HWND（attach 时缓存；覆盖层屏幕坐标换算用）。
    main_hwnd: Option<isize>,
    /// M2 弹幕覆盖窗口（GPU 路径独立线程；None = SW 路径或 spawn 失败）。
    overlay: Option<crate::overlay_window::OverlayHandle>,

    /// 会话重建续位：FileLoaded 后 seek 到此位置（覆盖 DB 续播值）。
    pending_seek: Option<f64>,
    /// 会话重建续位：FileLoaded 后恢复的暂停态。
    pending_pause: Option<bool>,
    /// 当前弹幕轨绑定关系 (video_path, danmaku_path)：延迟偏移按此对
    /// 存取 danmaku_bindings；换集清空，FileLoaded/手动加载时重建。
    /// 与 self.items[self.index] 独立——换集窗口期仍指向旧对，保存不错位。
    danmaku_track: Option<(std::path::PathBuf, std::path::PathBuf)>,
    last_progress_save: Instant,
}

/// 远程弹幕/字幕后台预取结果（后台线程 → UI 线程）。
struct RemotePrefetch {
    source_id: i64,
    video_path: PathBuf,
    /// 实际使用的弹幕 XML 路径与原始字节；None = 未找到/下载失败。
    danmaku: Option<(PathBuf, Vec<u8>)>,
    /// 已落盘缓存、可直接 `sub_add` 的字幕候选。
    subs: Vec<PreparedSub>,
}

/// 已准备（落盘缓存）的远程字幕候选。
struct PreparedSub {
    prepared: PathBuf,
    exact: bool,
    title: String,
}

/// 后台线程：下载弹幕 XML（绑定路径优先，失败回退同名 sibling）。
/// 与旧 UI 线程实现同语义，只是不再占用 UI 线程。
fn fetch_remote_danmaku(
    vfs: &dyn VfsSource,
    preferred: Option<PathBuf>,
    sibling: PathBuf,
) -> Option<(PathBuf, Vec<u8>)> {
    let mut used_path = preferred.unwrap_or_else(|| sibling.clone());
    match vfs.read_full(&used_path) {
        Ok(bytes) => Some((used_path, bytes)),
        Err(_) if used_path != sibling => match vfs.read_full(&sibling) {
            Ok(bytes) => {
                used_path = sibling;
                Some((used_path, bytes))
            }
            Err(_) => None,
        },
        Err(_) => None,
    }
}

/// 后台线程：逐个下载候选字幕并落盘缓存；失败项记日志并跳过。
fn fetch_remote_subs(
    vfs: &dyn VfsSource,
    candidates: &[SubtitleCandidate],
    cache_dir: Option<&Path>,
) -> Vec<PreparedSub> {
    let Some(cache_dir) = cache_dir else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cand in candidates {
        match vfs.read_full(&cand.path) {
            Ok(raw) => match crate::subtitles::prepare_remote_subtitle(&cand.path, &raw, cache_dir) {
                Ok(prepared) => out.push(PreparedSub {
                    prepared,
                    exact: cand.exact,
                    title: cand.title.clone(),
                }),
                Err(e) => tracing::warn!(
                    "{}",
                    crate::subtitles::describe_prepare_failure(&cand.path, &e)
                ),
            },
            Err(e) => {
                let e: anyhow::Error = e.into();
                tracing::warn!(
                    "{}",
                    crate::subtitles::describe_prepare_failure(&cand.path, &e)
                );
            }
        }
    }
    out
}

/// 视频面：mpv SW 软渲染（rgb0 CPU buffer）→ egui 纹理。
/// backend-wgpu 下 SW 渲染已迁至 `sw_render_thread`，此结构仅保留供
/// `VideoSurface::new()` 初始化占位（字段不再使用）。
#[cfg(feature = "backend-wgpu")]
#[allow(dead_code)]
struct VideoSurface {
    texture: Option<egui::TextureHandle>,
    buffer: Vec<u8>,
    size: [i32; 2],
}

/// 视频面：mpv GL 互操作 → GL FBO → egui 采样同一纹理（glow 后端）。
#[cfg(not(feature = "backend-wgpu"))]
struct VideoSurface {
    gl: std::sync::Arc<eframe::glow::Context>,
    tex: Option<eframe::glow::Texture>,
    fbo: Option<eframe::glow::Framebuffer>,
    tex_id: Option<egui::TextureId>,
    size: [i32; 2],
}

#[cfg(feature = "backend-wgpu")]
#[allow(dead_code)]
impl VideoSurface {
    fn new() -> Self {
        Self {
            texture: None,
            buffer: Vec::new(),
            size: [0, 0],
        }
    }

    fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture.as_ref().map(|t| t.id())
    }

    /// 尺寸变化时重建纹理与缓冲；返回是否发生了重建。
    fn ensure(&mut self, ctx: &egui::Context, size: [i32; 2]) -> bool {
        if size[0] <= 0 || size[1] <= 0 {
            return false;
        }
        if self.texture.is_some() && self.size == size {
            return false;
        }
        let (w, h) = (size[0] as usize, size[1] as usize);
        let image = egui::ColorImage {
            size: [w, h],
            source_size: egui::Vec2::new(w as f32, h as f32),
            pixels: vec![egui::Color32::BLACK; w * h],
        };
        match &mut self.texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("video-rgb0", image, egui::TextureOptions::LINEAR));
            }
        }
        self.buffer = vec![0u8; w * h * 4];
        self.size = size;
        true
    }

    /// 把 rgb0 buffer 整张上传纹理（alpha=255 时预乘==直通）。
    ///
    /// 单次遍历完成「X 字节置 255 + u8→Color32 转换」：不走
    /// `from_rgba_premultiplied`（其内部对 buffer 再拷贝一次），也不用
    /// `*mut u32` 重解释（`Vec<u8>` 抽象对齐仅 1，u32 视图是 UB）。
    fn upload(&mut self) {
        if let Some(tex) = &mut self.texture {
            debug_assert_eq!(
                self.buffer.len(),
                self.size[0] as usize * self.size[1] as usize * 4,
                "rgb0 buffer 尺寸与视频尺寸不一致（ensure 与 VideoReconfig 失配）"
            );
            let pixels: Vec<egui::Color32> = self
                .buffer
                .as_chunks::<4>()
                .0
                .iter()
                .map(|px| egui::Color32::from_rgb(px[0], px[1], px[2]))
                .collect();
            let image = egui::ColorImage {
                size: [self.size[0] as usize, self.size[1] as usize],
                source_size: egui::Vec2::new(self.size[0] as f32, self.size[1] as f32),
                pixels,
            };
            tex.set(image, egui::TextureOptions::LINEAR);
        }
    }
}

#[cfg(not(feature = "backend-wgpu"))]
impl VideoSurface {
    fn new(gl: std::sync::Arc<eframe::glow::Context>) -> Self {
        Self {
            gl,
            tex: None,
            fbo: None,
            tex_id: None,
            size: [0, 0],
        }
    }

    fn texture_id(&self) -> Option<egui::TextureId> {
        self.tex_id
    }

    /// 尺寸变化时重建 GL 纹理 + FBO；frame.register_native_glow_texture 桥接 egui。
    fn ensure(&mut self, frame: &mut eframe::Frame, size: [i32; 2]) -> bool {
        if size[0] <= 0 || size[1] <= 0 {
            return false;
        }
        if self.tex.is_some() && self.size == size {
            return false;
        }
        let gl = &self.gl;
        unsafe {
            let tex = match self.tex {
                Some(tex) => tex,
                None => {
                    let tex = gl.create_texture().expect("gl create_texture");
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MIN_FILTER,
                        glow::LINEAR as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MAG_FILTER,
                        glow::LINEAR as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_WRAP_S,
                        glow::CLAMP_TO_EDGE as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_WRAP_T,
                        glow::CLAMP_TO_EDGE as i32,
                    );
                    tex
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                size[0],
                size[1],
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            let fbo = match self.fbo {
                Some(fbo) => fbo,
                None => gl.create_framebuffer().expect("gl create_framebuffer"),
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            let complete =
                gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            if complete {
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if !complete {
                tracing::error!("video framebuffer incomplete");
                return false;
            }
            if self.tex_id.is_none() {
                self.tex_id = Some(frame.register_native_glow_texture(tex));
            }
            self.tex = Some(tex);
            self.fbo = Some(fbo);
            self.size = size;
        }
        true
    }

    /// glow 版 FBO id（mpv render 目标）。
    fn framebuffer_id(&self) -> Option<i32> {
        self.fbo.map(|fbo| fbo.0.get() as i32)
    }
}

#[cfg(not(feature = "backend-wgpu"))]
impl Drop for VideoSurface {
    fn drop(&mut self) {
        let gl = &self.gl;
        if let Some(fbo) = self.fbo.take() {
            unsafe { gl.delete_framebuffer(fbo) };
        }
        // R18：tex 已注册给 eframe（register_native_glow_texture 接管所有权），
        // eframe 退出时统一删除；此处再删会双删/悬垂名操作。
        self.tex.take();
    }
}

impl PlayerSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        egui_ctx: egui::Context,
        pending: PendingPlay,
        config: PlayerConfig,
        danmaku_cfg: dyplayer_store::DanmakuConfig,
        engine: &dyplayer_store::EngineConfig,
        store: std::sync::Arc<Store>,
        libmpv_path: Option<&std::path::Path>,
        #[cfg(not(feature = "backend-wgpu"))]
        gl: std::sync::Arc<eframe::glow::Context>,
    ) -> anyhow::Result<Self> {
        let lib = MpvLibrary::load(libmpv_path).context("loading libmpv")?;
        let mut extra_options: Vec<(String, String)> = Vec::new();
        if !config.pref_audio_lang.is_empty() {
            extra_options.push(("alang".into(), config.pref_audio_lang.clone()));
        }
        if !config.pref_sub_lang.is_empty() {
            extra_options.push(("slang".into(), config.pref_sub_lang.clone()));
        }
        if config.sub_disabled_by_default {
            extra_options.push(("sid".into(), "no".into()));
        }
        // 弹幕满帧率（display 配速方案，真机已验证）：mpv VO 以屏幕刷新率
        // 节拍呈现（重复帧也触发 update callback），驱动 eframe 满帧率重绘。
        // display-fps-override 用 QueryDisplayConfig 检测的精确信号频率
        // （59.94/119.88 分数值）；检测失败（远程会话/虚拟屏）不注入，
        // 退回 mpv 默认音频时钟（弹幕随视频帧率，正确但不够平滑）。
        //
        // 平滑配速可经设置面板开关（config.display_pacing）关闭改为音频时钟
        // 配速（省 CPU、高码率卡顿对照）；display-fps-override 仍照常注入以便
        // 运行时切回 display-resample（audio 模式下该值被 mpv 忽略）。
        if let Some(hz) = crate::refresh::cached_hz() {
            let vsync_mode = if config.display_pacing {
                "display-resample"
            } else {
                "audio"
            };
            tracing::info!("注入 mpv 配速: video-sync={vsync_mode} display-fps-override={hz:.6}");
            extra_options.push(("video-sync".into(), vsync_mode.into()));
            extra_options.push(("display-fps-override".into(), format!("{hz:.6}")));
        }
        // GPU wid 路径改用音频时钟配速：mpv 自管 swapchain 的
        // display-resample 强依赖 VO vsync 节拍，wid 宿主窗口（屏幕外/
        // 挂载前）无 DWM 呈现会让 mpv_initialize 死等（实测 hang）。
        // mpv 自管 vo 本身按显示器 vsync 呈现，audio 配速下视频平滑度
        // 由 vo 承担，弹幕满帧率改由 ui() 16ms 心跳驱动。
        if engine.render_path.eq_ignore_ascii_case("gpu") {
            if let Some(pos) = extra_options
                .iter()
                .position(|(name, _)| name == "video-sync")
            {
                extra_options[pos].1 = "audio".into();
            } else {
                extra_options.push(("video-sync".into(), "audio".into()));
            }
            if let Some(pos) = extra_options
                .iter()
                .position(|(name, _)| name == "display-fps-override")
            {
                extra_options.remove(pos);
            }
        }
        // 画质档位参数（scale/cscale/dscale/sigmoid/interpolation 等）：
        // 在用户 extra_args 之前注入，用户可覆盖。高档返回空（零回归）。
        // 低功耗档降级采样器避免弱 iGPU（N2940 Gen7）GPU 反压解码管线。
        let preset = QualityPreset::from_config(&engine.quality_preset);
        for (name, value) in preset.mpv_options() {
            extra_options.push(((*name).into(), (*value).into()));
        }
        // 引擎高级参数（name=value）最后注入，允许覆盖上面的行为设置。
        for arg in &engine.extra_args {
            match arg.split_once('=') {
                Some((name, value)) => {
                    extra_options.push((name.trim().into(), value.trim().into()));
                }
                None => tracing::warn!("忽略无法解析的引擎参数（应为 name=value）: {arg}"),
            }
        }
        let options = ClientOptions {
            hwdec: engine.hwdec.clone(),
            extra_options,
        };
        // 渲染路径分支（M1）：gpu = mpv 自管 D3D11（wid 宿主子窗口，
        // vo=gpu-next，无 render context）；sw = 现状（render API + 纹理
        // 上传）。宿主窗口创建失败时回退 SW 并 warn（渲染不可用不致命）。
        let host = if engine.render_path.eq_ignore_ascii_case("gpu") {
            crate::wid_host::VideoHost::create_hidden()
        } else {
            None
        };
        let gpu_wid = engine.render_path.eq_ignore_ascii_case("gpu") && host.is_some();
        if engine.render_path.eq_ignore_ascii_case("gpu") && host.is_none() {
            tracing::warn!("wid 宿主窗口创建失败，回退 SW 渲染路径");
        }
        let client = if gpu_wid {
            let host_hwnd = host.as_ref().map(|h| h.hwnd()).unwrap_or(0);
            std::sync::Arc::new(
                MpvClient::create_wid(lib, host_hwnd, "gpu-next", &options)
                    .context("creating mpv client (wid gpu)")?,
            )
        } else {
            std::sync::Arc::new(MpvClient::create(lib, &options).context("creating mpv client")?)
        };

        // 音量恢复（config.toml [player].volume）：注入 mpv 后由 FileLoaded
        // 事件读回同步（mpv 可能按 AO 能力 clamp，以读回值为准）。
        let startup_volume = config.volume.clamp(0, 100);
        if let Err(e) = client.set_property_i64("volume", startup_volume) {
            tracing::warn!("set startup volume failed: {e}");
        }

        client.observe_property(1, "time-pos", ObsFormat::Double)?;
        client.observe_property(2, "duration", ObsFormat::Double)?;
        client.observe_property(3, "speed", ObsFormat::Double)?;
        client.observe_property(4, "pause", ObsFormat::Flag)?;
        client.observe_property(5, "demuxer-cache-time", ObsFormat::Double)?;
        client.observe_property(6, "core-idle", ObsFormat::Flag)?;
        client.observe_property(7, "paused-for-cache", ObsFormat::Flag)?;

        // SW 路径独有：render context + update callback 驱动 eframe 重绘。
        // GPU wid 路径无 render context（mpv 自管呈现），重绘驱动改由
        // ui() 末尾的 request_repaint_after 心跳承担（见 ui()）。
        let render: Option<std::sync::Arc<MpvRenderContext>> = if gpu_wid {
            None
        } else {
            let mut render = unsafe {
                #[cfg(feature = "backend-wgpu")]
                {
                    MpvRenderContext::create_sw(&client)
                }
                #[cfg(not(feature = "backend-wgpu"))]
                {
                    MpvRenderContext::create(&client, crate::gl_loader::get_proc_address)
                }
            }
            .context("creating mpv render context")?;
            render.set_update_callback(Box::new({
                let repaint_ctx = egui_ctx.clone();
                move || {
                    // 只要一轮（request_repaint 每次画两轮让版面稳定用，
                    // 第二轮在帧还没到时间时只是多看一次；延迟非 0 只画一轮）。
                    repaint_ctx.request_repaint_after_for(
                        Duration::from_nanos(1),
                        egui::ViewportId::ROOT,
                    )
                }
            }));
            Some(std::sync::Arc::new(render))
        };

        // SW 渲染线程（仅 backend-wgpu + !gpu_wid）：render_sw + upload 搬离主线程。
        #[cfg(feature = "backend-wgpu")]
        let sw_thread: Option<SwRenderThread> = render
            .as_ref()
            .map(|r| SwRenderThread::spawn(r.clone(), egui_ctx.clone()));
        #[cfg(not(feature = "backend-wgpu"))]
        let sw_thread: Option<SwRenderThread> = None;

        let clock = std::sync::Arc::new(MediaClock::new());
        let (tx, rx) = mpsc::channel();
        let event_clock = std::sync::Arc::clone(&clock);
        let event_thread = client.spawn_event_loop(tx, move |change| match change {
            PropertyChange::TimePos(v) => event_clock.calibrate_smooth(*v),
            PropertyChange::Speed(v) => event_clock.set_speed(*v),
            PropertyChange::Paused(p) => event_clock.set_paused(*p),
            PropertyChange::CoreIdle(idle) => event_clock.set_stalled(*idle),
            _ => {}
        });

        #[cfg(feature = "backend-wgpu")]
        let video = VideoSurface::new();
        #[cfg(not(feature = "backend-wgpu"))]
        let video = VideoSurface::new(gl);
        let remote = pending.remote.clone();

        let mut session = Self {
            client: Some(client),
            render,
            sw_thread,
            clock,
            events: rx,
            event_thread: Some(event_thread),
            video,
            source_id: pending.source_id,
            http_headers: pending.http_headers.clone(),
            items: pending.items,
            index: pending.start_index,
            video_size: None,
            paused: false,
            duration: 0.0,
            chapters: Vec::new(),
            cache_time: 0.0,
            volume: startup_volume,
            status: String::new(),
            frame_counter: 0,
            last_fps_sample: Instant::now(),
            fps: 0.0,
            codec: String::new(),
            hwdec: String::new(),
            vo_drops: 0,
            last_stats_sample: Instant::now(),
            video_fps: 0.0,
            dm_frame_counter: 0,
            dm_fps: 0.0,
            ui_tick: 0,
            last_tick_log: Instant::now(),
            diag_last_upstream: 0,
            diag_last_gets: 0,
            diag_last_rate_tick: Instant::now(),
            last_reconfig_diag: Instant::now(),
            render_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            probe: None,
            osc_ui: crate::osc_state::OscUiState::default(),
            speed: 1.0,
            muted: false,
            loop_file: false,
            audio_tracks: Vec::new(),
            sub_tracks: Vec::new(),
            osc_theme_idx: 0,
            osc_layout: crate::osc_state::OscLayout::Default,
            osc_nibble: crate::osc_state::OscNibble::Gap,
            osc_fx_glow: false,
            osc_fx_size: true,
            osc_fx_color: true,
            fullscreen: false,
            fs_geom_logged: false,
            fs_pending: false,
            fs_guard: None,
            show_queue: false,
            settings_open: false,

            danmaku: {
                let mut overlay = DanmakuOverlay::new();
                overlay.apply_config(&danmaku_cfg);
                overlay
            },
            pending_danmaku: None,
            danmaku_track: None,
            ui_startup_view: dyplayer_store::StartupView::default(),
            ui_theme: dyplayer_store::ThemeMode::default(),
            remote,
            vfs: None,
            proxy: None,
            egui_ctx,
            remote_prefetch: None,
            prefetch_danmaku_overridden: false,

            requested_exit: false,
            store,
            config,
            engine: engine.clone(),
            hwdec_rebuild: false,
            gpu_wid,
            host,
            host_attached: false,
            main_hwnd: None,
            overlay: if gpu_wid {
                crate::overlay_window::OverlayHandle::spawn(danmaku_cfg.clone())
            } else {
                None
            },

            pending_seek: None,
            pending_pause: None,
            last_progress_save: Instant::now(),
        };
        session.load_current();
        Ok(session)
    }

    pub fn load(&mut self, pending: PendingPlay) {
        // 换源时丢弃旧 VFS/代理（同源换视频沿用，避免重建连接）。
        if pending.source_id != self.source_id {
            self.proxy = None;
            self.vfs = None;
        }
        self.remote = pending.remote.clone();
        self.source_id = pending.source_id;
        self.http_headers = pending.http_headers.clone();
        self.items = pending.items;
        self.index = pending.start_index;

        // 换队列后旧的手动弹幕请求过期（main.rs 拖放路径在 load 之后才设置新的）。
        self.pending_danmaku = None;
        self.duration = 0.0;
        // 新开播统一从播放态开始：mpv loadfile 不重置 pause 属性，
        // 需显式清掉上一会话遗留的暂停。
        if let Some(client) = &self.client
            && let Err(e) = client.set_property_string("pause", "no")
        {
            tracing::warn!("重置播放状态失败: {e}");
        }
        self.load_current();
    }

    /// 排队 XML 弹幕文件：下次 FileLoaded（同名自动加载）之后应用，有意覆盖同名轨。
    pub fn queue_danmaku_file(&mut self, path: PathBuf) {
        self.pending_danmaku = Some(path);
    }

    /// 立即载入 XML 弹幕文件（热替换当前弹幕轨）。
    pub fn load_danmaku_file(&mut self, path: &Path) -> Result<usize, String> {
        let n = self.danmaku.load_xml_file(path)?;
        self.overlay_load(crate::overlay_window::OverlayLoad::XmlPath(path.to_path_buf()));
        Ok(n)
    }

    /// 同步启动视图偏好（main.rs 创建/复用会话时注入；设置窗修改后写回）。
    pub fn set_ui_startup_view(&mut self, view: dyplayer_store::StartupView) {
        self.ui_startup_view = view;
    }

    /// 同步全局明暗主题偏好（main.rs 创建/复用会话时注入；设置窗修改后写回）。
    pub fn set_ui_theme(&mut self, mode: dyplayer_store::ThemeMode) {
        self.ui_theme = mode;
    }

    /// 全局明暗主题镜像（main.rs 每帧回读应用 visuals）。
    pub fn ui_theme(&self) -> dyplayer_store::ThemeMode {
        self.ui_theme
    }

    fn current_item(&self) -> Option<&MediaItem> {
        self.items.get(self.index)
    }

    /// 远程源：确保 VFS 与流代理就绪（懒构建，同一会话内复用）。
    fn ensure_remote(&mut self) -> Result<&StreamProxy, String> {
        if self.proxy.is_none() {
            let source = self
                .remote
                .clone()
                .ok_or_else(|| "远程播放缺少源配置".to_string())?;
            let (vfs, _) = source.open_vfs()?;
            let proxy = StreamProxy::spawn(vfs.clone()).map_err(|e| e.to_string())?;
            self.vfs = Some(vfs);
            self.proxy = Some(proxy);
        }
        Ok(self.proxy.as_ref().expect("proxy just ensured"))
    }

    fn load_current(&mut self) {
        self.video_size = None;
        self.chapters.clear();
        self.cache_time = 0.0;
        // 换集：上一集的远程预取结果作废（新 FileLoaded 会重新发起）。
        self.remote_prefetch = None;
        self.prefetch_danmaku_overridden = false;
        // 换集：先落旧 (视频, 弹幕) 对的最后偏移，再清弹幕轨与绑定关系
        self.save_danmaku_binding();
        self.danmaku.on_media_change();
        self.danmaku_track = None;
        // M2 覆盖层：换集窗口期清空（新 FileLoaded 到达后再加载）。
        self.overlay_load(crate::overlay_window::OverlayLoad::Clear);
        let Some(item) = self.current_item() else {
            return;
        };
        let item_path = item.path.clone();
        let item_name = item.name.clone();

        let play_url = if self.remote.is_some() {
            match self.ensure_remote() {
                Ok(proxy) => proxy.url_for(&item_path),
                Err(e) => {
                    tracing::warn!("remote source unavailable: {e}");
                    self.status = format!("连接远程源失败: {e}");
                    return;
                }
            }
        } else {
            item_path.to_string_lossy().into_owned()
        };
        if let Some(client) = &self.client {
            self.apply_lang_prefs(client);
            self.apply_http_headers(client);
            if let Err(e) = client.load_file(&play_url) {
                tracing::warn!("loadfile {play_url} failed: {e}");
                self.status = format!("加载失败: {e}");
                return;
            }
        }
        self.status = format!("加载 {item_name} …");
    }

    /// 播放前应用语言偏好（设置面板改动后对后续加载的视频生效）。
    fn apply_lang_prefs(&self, client: &std::sync::Arc<MpvClient>) {
        if !self.config.pref_audio_lang.is_empty()
            && let Err(e) = client.set_property_string("alang", &self.config.pref_audio_lang)
        {
            tracing::warn!("设置 alang 失败: {e}");
        }
        if !self.config.pref_sub_lang.is_empty()
            && let Err(e) = client.set_property_string("slang", &self.config.pref_sub_lang)
        {
            tracing::warn!("设置 slang 失败: {e}");
        }
        let sid = if self.config.sub_disabled_by_default {
            "no"
        } else {
            "auto"
        };
        if let Err(e) = client.set_property_string("sid", sid) {
            tracing::warn!("设置 sid 失败: {e}");
        }
    }

    /// 设置 mpv HTTP 请求头（串流播放自定义头，如 Referer）。
    /// 每次加载都重设：有头则逐个 set/add，无头则清空上次遗留。
    fn apply_http_headers(&self, client: &std::sync::Arc<MpvClient>) {
        match &self.http_headers {
            Some(headers) => {
                let lines: Vec<&str> = headers
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty() && l.contains(':'))
                    .collect();
                if let Some(first) = lines.first() {
                    if let Err(e) = client.command(&["set", "http-header-fields", first]) {
                        tracing::warn!("设置 http-header-fields 失败: {e}");
                    }
                    for h in &lines[1..] {
                        if let Err(e) = client.command(&["add", "http-header-fields", h]) {
                            tracing::warn!("追加 http-header-fields 失败: {e}");
                        }
                    }
                } else {
                    let _ = client.command(&["set", "http-header-fields", ""]);
                }
            }
            None => {
                let _ = client.command(&["set", "http-header-fields", ""]);
            }
        }
    }

    /// 应用视频配速开关（设置面板）：`display-resample`（按屏幕刷新率平滑配速，
    /// 弹幕满帧率）↔ `audio`（音频时钟配速，省 CPU）。运行时改 mpv 属性即时生效，
    /// 无需重建会话；display-fps-override 已在会话初始化按检测刷新率注入，
    /// audio 模式下被 mpv 忽略。
    fn apply_display_pacing(&self) {
        let Some(client) = &self.client else {
            return;
        };
        // GPU wid 路径创建期已强制 audio（wid 宿主窗口无 DWM 呈现时
        // display-resample 会让 mpv 等 vsync 挂起，见 new()）；运行时切回
        // 会重现同一 hang，故此处忽略并保持 audio。
        if self.gpu_wid {
            tracing::warn!("GPU wid 路径固定 audio 配速，忽略 video-sync 切换");
            return;
        }
        let mode = if self.config.display_pacing {
            "display-resample"
        } else {
            "audio"
        };
        if let Err(e) = client.set_property_string("video-sync", mode) {
            tracing::warn!("切换 video-sync={mode} 失败: {e}");
        } else {
            tracing::info!("切换视频配速: video-sync={mode}");
        }
    }

    /// 应用解码方式（设置面板三选一）。
    ///
    /// 会话重建型：mpv 运行时改 `hwdec` 只重建解码器与视频链，实测在 glow
    /// （GL render API）路径上会留下持久损坏的画面（横纹/色块）——exact
    /// seek 只能修解码侧的参考帧缺失，修不了渲染侧残留状态。重建会话让
    /// 解码器、VO、render context 一起从关键帧重新开始；位置/暂停态由
    /// App 的 `rebuild_session` 续接。
    fn apply_hwdec(&mut self) {
        self.hwdec_rebuild = true;
    }

    /// UI 线程：取走"解码方式已变更"请求（取即清）。
    pub fn take_hwdec_rebuild_request(&mut self) -> bool {
        std::mem::take(&mut self.hwdec_rebuild)
    }

    /// R20A：暴露 engine 镜像供 App 帧循环同步（避免 App 旧快照覆盖播放页修改）。
    pub fn engine_config(&self) -> &dyplayer_store::EngineConfig {
        &self.engine
    }

    /// R20A：暴露 player config 镜像供 App 帧循环同步。
    pub fn player_config(&self) -> &PlayerConfig {
        &self.config
    }


    pub fn current_title(&self) -> String {
        self.current_item()
            .map(|i| i.name.clone())
            .unwrap_or_default()
    }

    // R2：新 shell"正在播放"状态展示接线前保留。
    #[allow(dead_code)]
    pub fn source_id(&self) -> i64 {
        self.source_id
    }

    #[allow(dead_code)]
    pub fn media_time(&self) -> f64 {
        self.clock.now()
    }

    #[allow(dead_code)]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 自管全屏（fs_patch）状态：全屏时隐藏自定义标题栏与边缘 resize。
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    fn send_command(&self, args: &[&str]) {
        if let Some(client) = &self.client
            && let Err(e) = client.command(args)
        {
            tracing::warn!("mpv command {args:?} failed: {e}");
        }
    }

    pub fn toggle_pause(&mut self) {
        self.send_command(&["cycle", "pause"]);
    }

    /// 播放页退回媒体库前调用：仍在播放则先暂停（媒体库不承载播放态，
    /// 返回播放页时不触碰状态，保持暂停）。
    pub fn pause_if_playing(&mut self) {
        if !self.paused {
            self.send_command(&["set", "pause", "yes"]);
        }
    }

    /// 三面板互斥切换：打开目标面板时关闭其余两个；目标已开则全部关闭。
    fn toggle_panel(&mut self, target: PlayerPanel) {
        let is_open = match target {
            PlayerPanel::Queue => self.show_queue,
            PlayerPanel::DanmakuSettings => self.danmaku.settings_open,
            PlayerPanel::Settings => self.settings_open,
        };
        let (queue, danmaku, settings) = exclusive_panel_state(target, is_open);
        self.show_queue = queue;
        self.danmaku.settings_open = danmaku;
        self.settings_open = settings;
        // 面板与 OSC 轨道菜单互斥（避免叠加遮挡）。GPU 路径菜单状态在
        // 覆盖线程——经桥关闭（弹幕设置卡片同为覆盖层承载，一并关）。
        self.osc_ui.track_menu = None;
        if let Some(ov) = &self.overlay
            && let Ok(mut b) = ov.bridge().lock()
        {
            b.close_track_menu = true;
            b.danmaku_settings_close = true;
        }
    }

    /// 进入/退出自管全屏（不经过 winit 的 Fullscreen 命令——其样式切换
    /// 会触发本机 DWM/WGL 冻结，见 fs_patch.rs）。进入时关闭三面板
    /// （避免面板悬在无按钮的界面上），并唤起 OSC 提示退出途径
    /// （ESC / OSC 全屏按钮）。
    pub(crate) fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
        if fullscreen {
            self.show_queue = false;
            self.danmaku.settings_open = false;
            self.settings_open = false;
            self.osc_ui.track_menu = None;
            // GPU：覆盖线程菜单关闭（osc_wake 已含桥置位路径）。
            if let Some(ov) = &self.overlay
                && let Ok(mut b) = ov.bridge().lock()
            {
                b.close_track_menu = true;
                b.danmaku_settings_close = true;
            }
            self.osc_wake();
            self.fs_pending = true;
        } else {
            #[cfg(feature = "backend-wgpu")]
            {
                if let Some(guard) = self.fs_guard.take() {
                    crate::fs_patch::exit_fullscreen(guard);
                }
                self.fs_pending = false;
            }
            #[cfg(not(feature = "backend-wgpu"))]
            {
                // glow 版：退出最大化经 maybe_enter_fullscreen 发 Maximized(false)
                self.fs_pending = true;
            }
        }
    }

    /// 自管全屏进入：ui() 顶部执行（可从 frame 拿原生窗口句柄）。
    #[allow(unused_variables)]
    pub fn maybe_enter_fullscreen(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        #[cfg(feature = "backend-wgpu")]
        {
            if !self.fs_pending || !self.fullscreen || self.fs_guard.is_some() {
                return;
            }
            use raw_window_handle::HasWindowHandle;
            let Ok(handle) = frame.window_handle() else {
                return;
            };
            let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return;
            };
            let hwnd = windows::Win32::Foundation::HWND(win32.hwnd.get() as *mut core::ffi::c_void);
            self.fs_guard = Some(crate::fs_patch::enter_fullscreen(hwnd));
            self.fs_pending = false;
            self.fs_geom_logged = false;
            tracing::info!("FS_PATCH: entered self-managed fullscreen (THICKFRAME+NCCALCSIZE)");
        }
        #[cfg(not(feature = "backend-wgpu"))]
        {
            if !self.fs_pending {
                return;
            }
            // glow 版：窗口最大化（工作区，扣任务栏），避免 WGL 无边框全屏冻结
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.fullscreen));
            self.fs_pending = false;
            self.fs_geom_logged = false;
            tracing::info!("GLOW: fullscreen via window maximize (avoid WGL freeze)");
        }
    }


    /// GPU wid 路径宿主窗口挂载（ui() 首帧执行一次）：mpv wid 目标窗口
    /// 重父进 eframe 主窗口并转为可见子窗口，随后每帧由中央面板 align。
    pub fn maybe_attach_host(&mut self, frame: &eframe::Frame) {
        if !self.gpu_wid || self.host_attached {
            return;
        }
        use raw_window_handle::HasWindowHandle;
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return;
        };
        let parent = windows::Win32::Foundation::HWND(win32.hwnd.get() as *mut core::ffi::c_void);
        if let Some(host) = &mut self.host {
            host.attach(parent);
            self.host_attached = true;
            self.main_hwnd = Some(win32.hwnd.get());
            // 覆盖层挂 owner（owned window：永远浮在主窗口之上、随其
            // 最小化、不盖其他应用——修 NOACTIVATE 覆盖层被激活的主窗口
            // 压底的 z 序缺陷）。
            if let Some(ov) = &self.overlay
                && let Ok(mut b) = ov.bridge().lock()
            {
                b.owner_hwnd = win32.hwnd.get();
            }
            tracing::info!("WID_HOST: mpv 宿主子窗口已挂载进主窗口（GPU 渲染路径 M1）");
        }
    }

    /// M2 覆盖层桥接：推送弹幕加载请求（GPU wid 路径；主实例同步由调用点负责）。
    fn overlay_load(&self, req: crate::overlay_window::OverlayLoad) {
        if let Some(ov) = &self.overlay
            && let Ok(mut b) = ov.bridge().lock()
        {
            b.load_request = Some(req);
        }
    }

    /// 跨屏可见性（main.rs 在 Player↔Browse 切换时调用）。
    ///
    /// 离开播放页（active=false）：宿主子窗口是 Win32 WS_CHILD，永远绘制在
    /// 父窗口（eframe）客户区内容之上，present 擦不掉——必须主动收走，
    /// 否则视频画面残留悬浮在媒体库内容上。宿主窗口**移到屏幕外**而非
    /// SW_HIDE：与 M1 创建策略一致（AGENTS.md 坑 2：隐藏窗口的 mpv
    /// swapchain 呈现有死等风险，屏幕外可见最稳）；park 同时作废对齐缓存。
    /// 覆盖窗口走桥的 visible=false（DComp composition swapchain，SW_HIDE
    /// 安全，Occluded/Timeout 帧已安全跳过）。
    ///
    /// 回到播放页（active=true）：宿主由下一帧 ui() 的 align 拉回正确几何
    /// （park 已清 last_rect_px，"变化才提交"必然放行）；覆盖层恢复可见。
    pub fn set_player_screen_active(&mut self, active: bool) {
        if !active && self.fullscreen {
            // 离开播放页若处于全屏，先退出全屏恢复窗口几何
            // （SW/GPU 双路径都需要：全屏胶囊按钮返回媒体库的回归路径；
            // GPU 另有 WS_CHILD 宿主 park 后窗口大小无法恢复的约束）。
            self.set_fullscreen(false);
        }
        if !self.gpu_wid {
            return;
        }
        if !active {
            if let Some(host) = &mut self.host {
                host.park_offscreen();
            }
            if let Some(ov) = &self.overlay
                && let Ok(mut b) = ov.bridge().lock()
            {
                b.visible = false;
            }
            tracing::info!("GPU 宿主/覆盖层已收起（离开播放页）");
        } else if let Some(ov) = &self.overlay
            && let Ok(mut b) = ov.bridge().lock()
        {
            b.visible = true;
            tracing::info!("GPU 宿主/覆盖层恢复（回到播放页）");
        }
    }

    /// M2 覆盖层桥接：每帧写运行态（时间/暂停/即时项/OSC 快照）+ 屏幕矩形
    /// （覆盖窗口跟随视频矩形铺满区；主窗口客户区原点 + egui rect × ppp）。
    fn overlay_sync_frame(&self, media_time: f64, video_area: egui::Rect, ppp: f32) {
        let Some(ov) = &self.overlay else { return };
        let origin = self.main_hwnd.map(|h| {
            let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(
                    windows::Win32::Foundation::HWND(h as *mut _),
                    &mut pt,
                );
            }
            (pt.x, pt.y)
        });
        if let Ok(mut b) = ov.bridge().lock() {
            b.media_time = media_time;
            // R08：写时钟快照供覆盖线程自行外推（主线程卡顿时弹幕仍平滑推进）。
            b.clock_snapshot = self.clock.snapshot();
            b.paused = self.paused;
            b.ppp = ppp;
            // 组装快照后按覆盖层统计覆写弹幕帧率（GPU 下主线程 DanmakuOverlay
            // 不渲染、其 dm_fps 恒 0——HUD"弹幕"行取覆盖层 0.5s 窗口统计）。
            // 在锁内取 b.dm_fps 后写回，避免二次锁桥（Mutex 不可重入）。
            let mut st = self.osc_state();
            st.dm_fps = b.dm_fps;
            // 覆盖层弹幕设置卡片开态并入 panel_open（OSC keep 常显——
            // 卡片锚定"弹幕"按钮上方，OSC 淡出会让卡片失去锚点）。
            if b.danmaku_settings_open {
                st.panel_open = true;
            }
            b.osc_state = Some(st);
            // 弹幕设置卡片打开期间（卡片迁覆盖层）跳过 run_state 写入：
            // 滑条拖动中覆盖层本地即时项（opacity/delay/…）不能被主线程
            // 旧值覆盖回跳；关闭沿 SaveDanmakuConfig 回环后恢复同步。
            // 关闭沿 hold：覆盖线程已把卡片内最终即时项写回 run_state，
            // 本帧 SaveDanmakuConfig 尚未消费（exec 在 sync 之后），跳过
            // 写入防覆盖线程下帧读到旧值闪回；读到即清，下帧恢复正常。
            let settings_open = b.danmaku_settings_open;
            let run_state_hold = b.run_state_hold;
            if run_state_hold {
                b.run_state_hold = false;
            } else if !settings_open {
                b.run_state = crate::overlay_window::OverlayRunState {
                    enabled: self.danmaku.enabled,
                    show_top: self.danmaku.show_top_cfg,
                    show_bottom: self.danmaku.show_bottom_cfg,
                    opacity: self.danmaku.opacity,
                    delay_seconds: self.danmaku.delay_seconds,
                    gradient: self.danmaku.gradient_cfg,
                    gradient_inset: self.danmaku.gradient_inset_cfg,
                };
            }
            if let Some((ox, oy)) = origin {
                let l = ox + (video_area.left() * ppp).round() as i32;
                let t = oy + (video_area.top() * ppp).round() as i32;
                let r = ox + (video_area.right() * ppp).round() as i32;
                let bt = oy + (video_area.bottom() * ppp).round() as i32;
                b.screen_rect_px = [l, t, r.max(l), bt.max(t)];
            }
        }
    }

    /// 会话重建所需信息（渲染路径切换，main.rs 消费）：播放队列 + 当前
    /// 位置（mpv time-pos 优先，失败退 MediaClock）+ 暂停态。
    pub fn pending_for_rebuild(&self) -> (PendingPlay, f64, bool) {
        let pending = PendingPlay {
            source_id: self.source_id,
            items: self.items.clone(),
            start_index: self.index,
            remote: self.remote.clone(),
            http_headers: self.http_headers.clone(),
        };
        let pos = self
            .client
            .as_ref()
            .and_then(|c| c.get_property_f64("time-pos").ok())
            .filter(|t| t.is_finite() && *t > 0.0)
            .unwrap_or_else(|| self.clock.now().max(0.0));
        (pending, pos, self.paused)
    }

    /// 会话重建续位（main.rs 重建后回填）：FileLoaded 后 seek 到指定
    /// 位置并恢复暂停态（覆盖 load_current 的开播语义与 DB 续播值）。
    pub fn set_pending_resume(&mut self, pos: f64, paused: bool) {
        self.pending_seek = Some(pos);
        self.pending_pause = Some(paused);
    }

    pub fn seek_to(&mut self, t: f64) {
        let arg = format!("{t:.3}");
        self.send_command(&["seek", &arg, "absolute"]);
        self.clock.calibrate(t);
        self.status = format!("跳转 {:.1}s", t);
    }

    fn seek_rel(&mut self, delta: f64) {
        let arg = format!("{delta}");
        self.send_command(&["seek", &arg, "relative"]);
    }

    pub fn set_volume(&mut self, volume: i64) {
        self.volume = volume.clamp(0, 100);
        // 镜像进 config（内存）：落盘由退出/设置面板关闭时的 save_player_config 收口
        self.config.volume = self.volume;
        if let Some(client) = &self.client
            && let Err(e) = client.set_property_i64("volume", self.volume)
        {
            tracing::warn!("set volume failed: {e}");
        }
    }

    /// 设置播放速度（OSC 倍速按钮；mpv speed 事件会回写镜像，双写无害）。
    fn set_speed(&mut self, speed: f64) {
        self.speed = speed;
        let arg = format!("{speed}");
        self.send_command(&["set", "speed", &arg]);
    }

    /// 切换静音（OSC 音量图标）。
    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        let v = if self.muted { "yes" } else { "no" };
        self.send_command(&["set", "mute", v]);
    }

    /// 解除静音（拖动音量条时隐式触发）。
    fn unmute(&mut self) {
        if self.muted {
            self.muted = false;
            self.send_command(&["set", "mute", "no"]);
        }
    }

    /// 切换单集循环（OSC 循环按钮）。
    fn toggle_loop_file(&mut self) {
        self.loop_file = !self.loop_file;
        let v = if self.loop_file { "inf" } else { "no" };
        self.send_command(&["set", "loop-file", v]);
    }

    /// 切换音轨（None = 关闭音轨）；切换后刷新轨道镜像并绑定记忆。
    fn set_audio_track(&mut self, id: Option<i64>) {
        let v = id.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
        self.send_command(&["set", "aid", &v]);
        self.refresh_tracks();
        self.save_track_selection(dyplayer_store::TrackKind::Audio, id);
    }

    /// 切换字幕轨（None = 关闭字幕）；切换后刷新轨道镜像并绑定记忆。
    fn set_sub_track(&mut self, id: Option<i64>) {
        let v = id.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
        self.send_command(&["set", "sid", &v]);
        self.refresh_tracks();
        self.save_track_selection(dyplayer_store::TrackKind::Subtitle, id);
    }

    /// 轨道选择对级持久化（单击类：立即落盘，同 danmaku_bindings 时机
    /// 哲学）。下次播放同一视频时优先于语言偏好恢复（见
    /// restore_track_bindings）。外部文件轨（audio-add/sub-add）不写：
    /// 其 track id 跨会话不稳定，恢复侧 track-list 校验也会拒绝。
    fn save_track_selection(&mut self, kind: dyplayer_store::TrackKind, value: Option<i64>) {
        let Some(item) = self.current_item() else { return };
        let key = item.path.to_string_lossy().into_owned();
        if let Err(e) = self.store.save_track_selection(self.source_id, &key, kind, value) {
            tracing::warn!("轨道选择保存失败: {e}");
        }
    }

    /// FileLoaded 后恢复轨道绑定：(源, 视频) 对级——上次在该视频上的
    /// 显式选择覆盖 `apply_lang_prefs` 的 auto/no（load_current 先设、
    /// FileLoaded 后恢复，时序天然正确）。绑定 id 不在当前 track-list
    /// （文件版本变化/轨数变化）时静默放弃，保持语言偏好行为。
    fn restore_track_bindings(&mut self) {
        let Some(item) = self.current_item() else { return };
        let key = item.path.to_string_lossy().into_owned();
        let Ok(Some((aid, sid))) = self.store.load_track_binding(self.source_id, &key) else {
            return;
        };
        let audio_ids: Vec<i64> = self.audio_tracks.iter().map(|t| t.id).collect();
        let sub_ids: Vec<i64> = self.sub_tracks.iter().map(|t| t.id).collect();
        if let Some(client) = &self.client {
            if let TrackApply::Set(v) = track_apply_plan(aid, &audio_ids) {
                let s = v.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
                if let Err(e) = client.set_property_string("aid", &s) {
                    tracing::warn!("恢复音轨绑定失败: {e}");
                }
            }
            if let TrackApply::Set(v) = track_apply_plan(sid, &sub_ids) {
                let s = v.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
                if let Err(e) = client.set_property_string("sid", &s) {
                    tracing::warn!("恢复字幕轨绑定失败: {e}");
                }
            }
        }
        // 恢复改变了 selected 态——刷新轨道镜像（菜单当前选中标记）。
        self.refresh_tracks();
    }

    /// 加载外部音频文件（mpv audio-add select；文件名 stem 做轨道标题）。
    fn add_external_audio(&mut self, path: &std::path::Path) {
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("外部音频")
            .to_string();
        let p = path.to_string_lossy().into_owned();
        if let Some(client) = &self.client {
            match client.command(&["audio-add", &p, "select", &title]) {
                Ok(()) => {
                    self.status = format!("已加载外部音频：{title}");
                    self.refresh_tracks();
                }
                Err(e) => {
                    tracing::warn!("audio-add {p} failed: {e}");
                    self.status = format!("外部音频加载失败：{e}");
                }
            }
        }
    }

    /// 加载外部字幕文件（编码规范化 → mpv sub-add select）。
    fn add_external_subtitle(&mut self, path: &std::path::Path) {
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("外部字幕")
            .to_string();
        let cache = match crate::subtitles::subtitle_cache_dir() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("subtitle cache dir unavailable: {e}");
                self.status = format!("字幕加载失败：{e}");
                return;
            }
        };
        match crate::subtitles::prepare_subtitle(path, &cache) {
            Ok(prepared) => {
                let p = prepared.to_string_lossy().into_owned();
                if let Some(client) = &self.client {
                    match client.sub_add(&p, true, &title) {
                        Ok(()) => {
                            self.status = format!("已加载外部字幕：{title}");
                            self.refresh_tracks();
                        }
                        Err(e) => {
                            tracing::warn!("sub-add {p} failed: {e}");
                            self.status = format!("字幕加载失败：{e}");
                        }
                    }
                }
            }
            Err(e) => {
                let msg = crate::subtitles::describe_prepare_failure(path, &e);
                tracing::warn!("{msg}");
                self.status = msg;
            }
        }
    }

    /// 从 mpv 拉取 track-list 并按 audio/sub 分类镜像（FileLoaded 与切换轨后）。
    fn refresh_tracks(&mut self) {
        let Some(client) = &self.client else { return };
        let tracks = client.get_track_list().unwrap_or_default();
        self.audio_tracks = tracks
            .iter()
            .filter(|t| t.kind == "audio")
            .cloned()
            .collect();
        self.sub_tracks = tracks.iter().filter(|t| t.kind == "sub").cloned().collect();
    }

    /// 唤起 OSC（点击画面 / 快捷键 / 进入全屏时）。
    fn osc_wake(&mut self) {
        self.osc_ui.last_activity = Instant::now();
        self.osc_ui.shown = true;
        // GPU 路径：显隐状态机在覆盖线程——经桥唤醒（SW 下桥不存在）。
        if let Some(ov) = &self.overlay
            && let Ok(mut b) = ov.bridge().lock()
        {
            b.osc_wake = true;
        }
    }

    /// 组装 OSC 渲染快照（osc_state.rs 契约）：SW 本地传参 / GPU 写桥。
    fn osc_state(&self) -> crate::osc_state::OscState {
        crate::osc_state::OscState {
            title: self.current_title(),
            duration: self.duration,
            position: self.clock.now(),
            paused: self.paused,
            volume: self.volume,
            muted: self.muted,
            speed: self.speed,
            fullscreen: self.fullscreen,
            loop_file: self.loop_file,
            can_prev: self.index > 0,
            can_next: self.index + 1 < self.items.len(),
            audio_tracks: self.audio_tracks.clone(),
            sub_tracks: self.sub_tracks.clone(),
            chapters: self.chapters.clone(),
            show_hud: self.config.show_stats_hud,
            codec: self.codec.clone(),
            hwdec: self.hwdec.clone(),
            vo_drops: self.vo_drops,
            video_fps: self.video_fps,
            cache_time: self.cache_time,
            ui_fps: self.fps,
            dm_fps: self.dm_fps,

            danmaku_enabled: self.danmaku.enabled,
            video_size: self.video_size,
            status: self.status.clone(),
            queue_pos: self.index + 1,
            queue_len: self.items.len(),
            queue_open: self.show_queue,
            queue_titles: self.items.iter().map(|i| i.name.clone()).collect(),
            settings_open: self.settings_open,
            player_cfg: self.config.clone(),
            engine_hwdec: self.engine.hwdec.clone(),
            engine_render_path: self.engine.render_path.clone(),
            ui_theme: self.ui_theme,
            startup_view: self.ui_startup_view,
            danmaku_delay: self.danmaku.delay_seconds,
            panel_open: self.settings_open || self.danmaku.settings_open || self.show_queue,
            seek_step_back: self.config.seek_step_back as f64,
            seek_step_fwd: self.config.seek_step_fwd as f64,
            theme_idx: self.osc_theme_idx,
            layout: self.osc_layout,
            fx_size: self.osc_fx_size,
            fx_color: self.osc_fx_color,
            fx_glow: self.osc_fx_glow,
            nibble: self.osc_nibble,
        }
    }

    /// 执行 OSC 命令（SW 本地 / GPU 桥回传共用；mpv 与 store 只在主线程）。
    fn exec_osc_cmds(&mut self, cmds: Vec<crate::osc_state::OscCmd>) {
        for cmd in cmds {
            use crate::osc_state::OscCmd;
            match cmd {
                OscCmd::TogglePause => self.toggle_pause(),
                OscCmd::SeekTo(t) => self.seek_to(t),
                OscCmd::SeekRel(d) => self.seek_rel(d),
                OscCmd::SetVolume(v) => {
                    self.set_volume(v);
                    self.unmute();
                }
                OscCmd::ToggleMute => self.toggle_mute(),
                OscCmd::SetSpeed(s) => self.set_speed(s),
                OscCmd::ToggleFullscreen => self.set_fullscreen(!self.fullscreen),
                OscCmd::ToggleLoopFile => self.toggle_loop_file(),
                OscCmd::TakeScreenshot => self.take_screenshot(),
                OscCmd::ExitToLibrary => self.requested_exit = true,
                OscCmd::SkipPrev => {
                    let _ = self.advance(-1);
                }
                OscCmd::SkipNext => {
                    let _ = self.advance(1);
                }
                OscCmd::ToggleHud => {
                    self.config.show_stats_hud = !self.config.show_stats_hud;
                    self.save_player_config();
                }
                OscCmd::SelectAudio(id) => self.set_audio_track(id),
                OscCmd::SelectSub(id) => self.set_sub_track(id),
                OscCmd::PickAudioFile => {
                    // rfd 主线程弹窗（覆盖线程不能弹；无扩展过滤：mpv 容器极广）。
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("选择音频文件")
                        .pick_file()
                    {
                        self.add_external_audio(&path);
                    }
                }
                OscCmd::PickSubFile => {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("选择字幕文件")
                        .pick_file()
                    {
                        self.add_external_subtitle(&path);
                    }
                }
                OscCmd::TogglePanelQueue => self.toggle_panel(PlayerPanel::Queue),
                OscCmd::TogglePanelSettings => self.toggle_panel(PlayerPanel::Settings),
                OscCmd::PlayIndex(i) => self.play_index(i),
                OscCmd::SetPlayerConfig(cfg) => {
                    let pacing_changed = cfg.display_pacing != self.config.display_pacing;
                    self.config = cfg;
                    if pacing_changed {
                        self.apply_display_pacing();
                    }
                    self.save_player_config();
                }
                OscCmd::SetHwdec(h) => {
                    if h != self.engine.hwdec {
                        self.engine.hwdec = h;
                        // 落盘与重建由 App 帧循环在会话重建时执行。
                        self.apply_hwdec();
                    }
                }
                OscCmd::SetTheme(mode) => {
                    self.ui_theme = mode;
                    save_theme(mode);
                }
                OscCmd::SetStartupView(view) => {
                    self.ui_startup_view = view;
                    save_startup_view(view);
                }
                OscCmd::SetDanmakuDelay(d) => {
                    self.danmaku.delay_seconds = d;
                }
                OscCmd::SetOscPrefs {
                    theme_idx,
                    layout,
                    nibble,
                    fx_glow,
                    fx_size,
                    fx_color,
                } => {
                    self.osc_theme_idx = theme_idx;
                    self.osc_layout = layout;
                    self.osc_nibble = nibble;
                    self.osc_fx_glow = fx_glow;
                    self.osc_fx_size = fx_size;
                    self.osc_fx_color = fx_color;
                }
                OscCmd::ToggleDanmakuSettings => {
                    if self.gpu_wid {
                        // GPU：卡片在覆盖层（主线程画会被宿主盖住）——经桥
                        // 切换覆盖层实例的开态。
                        if let Some(ov) = &self.overlay
                            && let Ok(mut b) = ov.bridge().lock()
                        {
                            b.danmaku_settings_toggle = true;
                        }
                    } else {
                        self.toggle_panel(PlayerPanel::DanmakuSettings);
                    }
                }
                OscCmd::ToggleDanmakuEnabled => {
                    self.danmaku.enabled = !self.danmaku.enabled;
                    self.status = if self.danmaku.enabled {
                        "弹幕开".into()
                    } else {
                        "弹幕关".into()
                    };
                    // 弹幕开关持久化（config.toml [danmaku] 节；低频单击）。
                    self.save_danmaku_config();
                }
                OscCmd::SaveDanmakuConfig(cfg) => {
                    // 弹幕设置卡片关闭沿（GPU）：覆盖层导出的完整配置——
                    // 镜像到主线程实例 + 落盘 + push 回环（cfg_version++
                    // 覆盖层幂等 apply 同值）。delay 是会话级即时项不在
                    // DanmakuConfig，经桥 danmaku_delay 同环回传，防关闭
                    // 后被主线程旧值打回。
                    self.danmaku.apply_config(&cfg);
                    if let Some(ov) = &self.overlay
                        && let Ok(mut b) = ov.bridge().lock()
                        && let Some(d) = b.danmaku_delay.take()
                    {
                        self.danmaku.delay_seconds = d;
                    }
                    self.save_danmaku_config();
                }
                OscCmd::PickDanmakuFile => {
                    // 覆盖线程 rfd 请求：主线程弹窗，结果经桥回覆盖层加载。
                    let picked = rfd::FileDialog::new()
                        .set_title("选择弹幕文件")
                        .add_filter("弹幕文件 (*.xml)", &["xml"])
                        .pick_file();
                    if let Some(ov) = &self.overlay
                        && let Ok(mut b) = ov.bridge().lock()
                    {
                        b.danmaku_file_result = Some(picked);
                    }
                }
                OscCmd::PickDanmakuFont => {
                    let picked = rfd::FileDialog::new()
                        .set_title("选择字体文件")
                        .add_filter("字体文件", &["ttf", "otf", "ttc"])
                        .pick_file();
                    if let Some(ov) = &self.overlay
                        && let Ok(mut b) = ov.bridge().lock()
                    {
                        b.font_file_result = Some(picked);
                    }
                }
                OscCmd::BindDanmakuFile(xml) => {
                    // 覆盖层卡片内手动加载成功：与 SW 的 manual_pick 消费
                    // 同语义（绑定 + 覆盖层同步加载）。
                    self.overlay_load(crate::overlay_window::OverlayLoad::XmlPath(xml.clone()));
                    self.bind_danmaku(&xml);
                }
            }
        }
    }

    /// 播放界面键盘快捷键：空格=播放/暂停，←/→=快退/快进，↑/↓=音量，Tab=媒体信息块。
    /// 键盘完全接管：清空控件焦点并重置导航方向（焦点导航在 begin_pass 基于原始输入
    /// 运行，须显式重置），事件被就地消费，避免再触发聚焦控件（空格=点击聚焦按钮）。
    /// 当前播放界面无文本输入控件；若未来加入 TextEdit，需在聚焦时让位。
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // 主线程残留焦点（如从 Browse 搜索框带入）清掉。播放页主线程无
        // TextEdit，focused() 应恒 false；若有残留则清之，不 return——
        // 继续检测覆盖层焦点（GPU 路径 TextEdit 在覆盖层）。
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
            m.move_focus(egui::FocusDirection::None);
        });
        // R19：GPU 覆盖层有焦点控件时抑制主线程快捷键。
        if self.overlay.as_ref().is_some_and(|ov| {
            ov.bridge().lock().map(|b| b.overlay_has_focus).unwrap_or(false)
        }) {
            // 键盘事件只到主窗口 egui ctx，覆盖层 ctx 收不到——转发
            // Key/Text/Ime 事件经桥到覆盖线程（TextEdit 输入靠此通路）。
            let mut key_events: Vec<egui::Event> = Vec::new();
            ctx.input_mut(|i| {
                i.events.retain(|e| {
                    if matches!(
                        e,
                        egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Ime(_)
                    ) {
                        key_events.push(e.clone());
                        false
                    } else {
                        true
                    }
                });
            });
            if !key_events.is_empty()
                && let Some(ov) = &self.overlay
                && let Ok(mut b) = ov.bridge().lock()
            {
                b.key_events.append(&mut key_events);
            }
            return;
        }
        /// 取走指定按键事件；repeat 事件一律消费以防聚焦控件响应，
        /// 但仅当 `allow_repeat` 时才触发动作（避免按住空格狂切暂停）。
        fn take_key(i: &mut egui::InputState, key: egui::Key, allow_repeat: bool) -> bool {
            let mut pressed = false;
            i.events.retain(|e| {
                if let egui::Event::Key {
                    key: k,
                    pressed: true,
                    repeat,
                    ..
                } = e
                    && *k == key
                {
                    if !*repeat || allow_repeat {
                        pressed = true;
                    }
                    return false;
                }
                true
            });
            pressed
        }
        let (
            mut space,
            mut left,
            mut right,
            mut up,
            mut down,
            mut tab,
            mut escape,
            mut next_ep,
            mut prev_ep,
            mut danmaku_key,
            mut screenshot,
        ) = (
            false, false, false, false, false, false, false, false, false, false, false,
        );
        ctx.input_mut(|i| {
            space = take_key(i, egui::Key::Space, false);
            left = take_key(i, egui::Key::ArrowLeft, true);
            right = take_key(i, egui::Key::ArrowRight, true);
            up = take_key(i, egui::Key::ArrowUp, true);
            down = take_key(i, egui::Key::ArrowDown, true);
            tab = take_key(i, egui::Key::Tab, false);
            next_ep = take_key(i, egui::Key::N, false);
            prev_ep = take_key(i, egui::Key::P, false);
            danmaku_key = take_key(i, egui::Key::D, false);
            screenshot = take_key(i, egui::Key::S, false);
            // ESC 仅全屏时消费（退出全屏）；非全屏留给 egui 默认行为。
            // 例外：GPU 弹幕设置卡片开着时（卡片在覆盖层 ctx，键盘事件
            // 只到主窗口）——消费并经桥关闭卡片。
            if self.fullscreen {
                escape = take_key(i, egui::Key::Escape, false);
            } else if self.gpu_wid {
                let overlay_settings_open = self
                    .overlay
                    .as_ref()
                    .and_then(|ov| ov.bridge().lock().ok())
                    .is_some_and(|b| b.danmaku_settings_open);
                if overlay_settings_open || self.settings_open || self.show_queue {
                    escape = take_key(i, egui::Key::Escape, false);
                }
            }
        });
        if escape {
            if self.fullscreen {
                self.set_fullscreen(false);
            } else if self.settings_open {
                self.settings_open = false;
            } else if self.show_queue {
                self.show_queue = false;
            } else if let Some(ov) = &self.overlay
                && let Ok(mut b) = ov.bridge().lock()
            {
                b.danmaku_settings_close = true;
            }
        }
        if space {
            self.toggle_pause();
        }
        if left {
            self.seek_rel(-(self.config.seek_step_back as f64));
            self.status = format!("后退 {}s", self.config.seek_step_back);
        }
        if right {
            self.seek_rel(self.config.seek_step_fwd as f64);
            self.status = format!("前进 {}s", self.config.seek_step_fwd);
        }
        if up || down {
            let delta = if up {
                VOLUME_KEY_STEP
            } else {
                -VOLUME_KEY_STEP
            };
            self.set_volume(self.volume + delta);
            self.status = format!("音量 {}", self.volume);
        }
        if tab {
            self.config.show_stats_hud = !self.config.show_stats_hud;
            self.save_player_config();
        }
        if next_ep {
            if self.advance(1) {
                self.status = "下一集".into();
            } else {
                self.status = "已是最后一集".into();
            }
        }
        if prev_ep {
            if self.advance(-1) {
                self.status = "上一集".into();
            } else {
                self.status = "已是第一集".into();
            }
        }
        if danmaku_key {
            self.danmaku.enabled = !self.danmaku.enabled;
            self.status = if self.danmaku.enabled {
                "弹幕开".into()
            } else {
                "弹幕关".into()
            };
            self.save_danmaku_config();
        }
        if screenshot {
            self.take_screenshot();
        }
        if space || left || right || up || down || tab {
            self.osc_wake();
        }
    }

    /// S 键截图：保存到数据目录 screenshots/（含 mpv 字幕渲染，不含 UI/弹幕）。
    fn take_screenshot(&mut self) {
        let Some(client) = &self.client else {
            return;
        };
        let dir = dyplayer_store::AppConfig::data_dir()
            .map(|d| d.join("screenshots"))
            .unwrap_or_else(|_| std::path::PathBuf::from("screenshots"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.status = format!("截图目录创建失败: {e}");
            return;
        }
        let name: String = self
            .current_title()
            .chars()
            .map(|c| {
                if c.is_whitespace()
                    || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let name = if name.is_empty() {
            "screenshot".to_string()
        } else {
            name
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("{name}_{stamp}.png"));
        match client.command(&["screenshot-to-file", &path.to_string_lossy(), "subtitles"]) {
            Ok(()) => self.status = format!("截图已保存 {}", path.display()),
            Err(e) => self.status = format!("截图失败: {e}"),
        }
    }

    pub fn play_index(&mut self, index: usize) {
        if index < self.items.len() && index != self.index {
            self.index = index;
            self.load_current();
        }
    }

    pub fn advance(&mut self, delta: i64) -> bool {
        let next = self.index as i64 + delta;
        if next < 0 || next >= self.items.len() as i64 {
            return false;
        }
        self.index = next as usize;
        self.load_current();
        true
    }

    fn save_progress(&mut self) {
        let Some(item) = self.current_item() else {
            return;
        };
        if self.duration <= 0.0 {
            return;
        }
        let position = self.clock.now();
        let path = item.path.to_string_lossy().into_owned();
        if let Err(e) = self.store.upsert_play_position(
            self.source_id,
            &path,
            &item.name,
            position,
            self.duration,
            self.http_headers.as_deref(),
        ) {
            tracing::warn!("save progress failed: {e}");
        }
    }

    /// 查询并应用 (视频, 弹幕文件) 对的持久化延迟偏移；无记录归零
    /// （偏移属于"该视频配该弹幕文件"的校准值，换轨/换集不沿用旧值）。
    fn restore_danmaku_offset(&mut self, video_key: &str, xml: &Path) {
        let offset = self
            .store
            .load_danmaku_offset(self.source_id, video_key, &xml.to_string_lossy())
            .ok()
            .flatten()
            .unwrap_or(0.0);
        self.danmaku.delay_seconds = offset;
    }

    /// 绑定手动指定的弹幕文件到当前视频：恢复该对历史偏移并立即落库
    /// （记住手动选择——下次打开该视频优先于同名 xml 加载）。
    fn bind_danmaku(&mut self, xml: &Path) {
        let Some(item) = self.current_item() else {
            return;
        };
        let video_key = item.path.to_string_lossy().into_owned();
        let xml_key = xml.to_string_lossy().into_owned();
        self.danmaku_track = Some((item.path.clone(), xml.to_path_buf()));
        self.restore_danmaku_offset(&video_key, xml);
        if let Err(e) = self.store.upsert_danmaku_binding(
            self.source_id,
            &video_key,
            &xml_key,
            self.danmaku.delay_seconds,
        ) {
            tracing::warn!("save danmaku binding failed: {e}");
        }
    }

    /// 周期/退出/换集时保存当前 (视频, 弹幕) 对的延迟偏移。
    fn save_danmaku_binding(&mut self) {
        let Some((video, xml)) = self.danmaku_track.clone() else {
            return;
        };
        let video_key = video.to_string_lossy().into_owned();
        let xml_key = xml.to_string_lossy().into_owned();
        if let Err(e) = self.store.upsert_danmaku_binding(
            self.source_id,
            &video_key,
            &xml_key,
            self.danmaku.delay_seconds,
        ) {
            tracing::warn!("save danmaku binding failed: {e}");
        }
    }

    /// 将弹幕全局设置写回 config.toml 的 [danmaku] 段（读改写，保留其他配置）。
    /// GPU wid 路径同时同步覆盖层（布局项触发异步重排；即时项由每帧
    /// overlay_sync_frame 的 run_state 同步）。
    fn save_danmaku_config(&self) {
        let cfg = self.danmaku.export_config();
        if let Some(ov) = &self.overlay {
            ov.push_config(&cfg);
        }
        let Ok(dir) = dyplayer_store::AppConfig::config_dir() else {
            return;
        };
        match dyplayer_store::AppConfig::load_or_default() {
            Ok(mut loaded) => {
                loaded.danmaku = cfg;
                if let Err(e) = loaded.save(&dir.join("config.toml")) {
                    tracing::warn!("保存弹幕配置失败: {e}");
                }
            }
            Err(e) => tracing::warn!("读取配置失败，无法保存弹幕设置: {e}"),
        }
    }

    pub fn poll_events(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.set_phase(PH_POLL);
        while let Ok(event) = self.events.try_recv() {
            self.set_phase(PH_POLL);
            match event {
                EngineEvent::FileLoaded => {
                    self.set_phase(PH_FILE_LOADED);
                    if let Some(size) = read_video_size(&client) {
                        self.video_size = Some(size);
                        self.status = format!("正在播放 {}x{}", size[0], size[1]);
                    } else {
                        self.status = "正在播放".into();
                    }
                    self.duration = client.get_property_f64("duration").unwrap_or(0.0);
                    self.chapters = client.get_chapter_list().unwrap_or_default();
                    self.cache_time = 0.0;
                    self.paused = client.get_property_flag("pause").unwrap_or(false);
                    self.volume = client.get_property_i64("volume").unwrap_or(100);
                    // mpv 读回的音量才是真实生效值（AO 能力可能 clamp）——同步镜像
                    self.config.volume = self.volume;
                    self.speed = client.get_property_f64("speed").unwrap_or(1.0);
                    self.muted = client.get_property_flag("mute").unwrap_or(false);
                    self.loop_file = client
                        .get_property_string("loop-file")
                        .is_ok_and(|s| s != "no");
                    self.refresh_tracks();
                    // 轨道绑定恢复（同视频上次选择优先于语言偏好）。
                    self.restore_track_bindings();
                    // 换集后轨道列表已变，残留的轨道菜单关闭
                    //（GPU：覆盖线程实例经桥）。
                    self.osc_ui.track_menu = None;
                    if let Some(ov) = &self.overlay
                        && let Ok(mut b) = ov.bridge().lock()
                    {
                        b.close_track_menu = true;
                        b.danmaku_settings_close = true;
                    }
                    self.refresh_decode_stats(&client);
                    self.log_playback_diag(&client, "FileLoaded");
                    // 差分基线重置：换集后从零观察本期上游吞吐。
                    if let Some(p) = &self.proxy {
                        let s = p.stats();
                        self.diag_last_upstream = s.upstream_bytes();
                        self.diag_last_gets = s.gets();
                        self.diag_last_rate_tick = Instant::now();
                    }
                    self.clock.set_paused(self.paused);
                    if let Ok(t) = client.get_property_f64("time-pos") {
                        self.clock.calibrate(t);
                    }
                    if let Some(item) = self.current_item().map(|i| i.path.clone()) {
                        if self.remote.is_some() {
                            // 远程源：弹幕/字幕由后台线程下载（UI 线程同步网络 IO
                            // 会在服务器挂起时冻结窗口——见 spawn_remote_prefetch）。
                            self.spawn_remote_prefetch(&item);
                        } else {
                            // 绑定优先：上次绑定的弹幕文件（手动指定优先于同名 xml）
                            let video_key = item.to_string_lossy().into_owned();
                            let preferred = self
                                .store
                                .load_bound_danmaku_path(self.source_id, &video_key)
                                .ok()
                                .flatten()
                                .map(std::path::PathBuf::from);
                            if self.gpu_wid {
                                // R12-C1：GPU 路径主实例不渲染弹幕，跳过 XML
                                // 读取/解析/prepare（覆盖层处理）。仍确定弹幕
                                // 路径用于 track/偏移恢复。
                                let xml = preferred
                                    .as_ref()
                                    .filter(|p| p.exists())
                                    .cloned()
                                    .or_else(|| {
                                        let same =
                                            crate::danmaku_ui::xml_path_for_video(&item);
                                        same.exists().then_some(same)
                                    });
                                if let Some(xml) = xml {
                                    self.danmaku_track =
                                        Some((item.clone(), xml.clone()));
                                    self.restore_danmaku_offset(&video_key, &xml);
                                }
                            } else if let Some(xml) = self
                                .danmaku
                                .on_file_loaded(&item, preferred.as_deref())
                            {
                                self.danmaku_track = Some((item.clone(), xml.clone()));
                                self.restore_danmaku_offset(&video_key, &xml);
                            }
                            // M2 覆盖层：同名自动加载（绑定优先语义与主实例一致）。
                            self.overlay_load(crate::overlay_window::OverlayLoad::Auto {
                                video: item.clone(),
                                preferred,
                            });
                            self.load_local_subs();
                        }
                    }
                    // 拖入的 XML 弹幕在同名自动加载之后应用（有意覆盖同名轨）。
                    if let Some(xml) = self.pending_danmaku.take() {
                        if self.gpu_wid {
                            // R12-C1：GPU 路径跳过主实例解析（覆盖层处理）。
                            self.bind_danmaku(&xml);
                            self.prefetch_danmaku_overridden = true;
                            self.overlay_load(
                                crate::overlay_window::OverlayLoad::XmlPath(xml.clone()),
                            );
                        } else {
                            match self.danmaku.load_xml_file(&xml) {
                                Ok(n) => {
                                    self.status =
                                        format!("{} · 弹幕 {n} 条", self.status);
                                    self.bind_danmaku(&xml);
                                    self.prefetch_danmaku_overridden = true;
                                    self.overlay_load(
                                        crate::overlay_window::OverlayLoad::XmlPath(
                                            xml.clone(),
                                        ),
                                    );
                                }
                                Err(e) => tracing::warn!("拖入弹幕加载失败: {e}"),
                            }
                        }
                    }
                    self.try_resume();
                    // 会话重建续位（渲染路径切换）：显式位置/暂停态覆盖
                    // DB 续播与 load_current 的开播语义。
                    if let Some(t) = self.pending_seek.take()
                        && t.is_finite()
                        && t > 0.0
                    {
                        let arg = format!("{t:.3}");
                        self.send_command(&["seek", &arg, "absolute"]);
                        self.clock.calibrate(t);
                        self.status = format!("从 {} 续播（重建会话）", format_time(t));
                    }
                    if self.pending_pause.take().unwrap_or(false) {
                        self.send_command(&["set", "pause", "yes"]);
                        self.paused = true;
                        self.clock.set_paused(true);
                    }
                }
                EngineEvent::VideoReconfig => {
                    self.set_phase(PH_RECONFIG);
                    if let Some(size) = read_video_size(&client) {
                        if self.video_size != Some(size) {
                            self.status = format!("正在播放 {}x{}", size[0], size[1]);
                        }
                        self.video_size = Some(size);
                    }
                    // hwdec 切换尺寸不变时纹理仍须重建+清屏，否则残留帧撕裂错乱。
                    if let Some(t) = &self.sw_thread {
                        t.bridge()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .request_reconfig();
                    }
                    // hwdec-current 在视频轨就绪（reconfig）后才最终确定。
                    self.refresh_decode_stats(&client);
                    // 限流 dump：同文件 reconfig 连发（尺寸/轨道切换）只打一条。
                    if self.last_reconfig_diag.elapsed() >= Duration::from_secs(5) {
                        self.last_reconfig_diag = Instant::now();
                        self.log_playback_diag(&client, "Reconfig");
                    }
                }
                EngineEvent::Seek | EngineEvent::PlaybackRestart => {
                    self.set_phase(PH_TIME_POS);
                    if let Ok(t) = client.get_property_f64("time-pos") {
                        self.clock.calibrate(t);
                    }
                }
                EngineEvent::EndFile { reason, error } => {
                    self.on_end_file(reason, error);
                }
                EngineEvent::PropertyChange(change) => match change {
                    PropertyChange::Paused(p) => self.paused = p,
                    PropertyChange::Duration(d) => self.duration = d,
                    PropertyChange::Speed(s) => self.speed = s,
                    PropertyChange::DemuxerCacheTime(t) => self.cache_time = t,
                    _ => {}
                },
                EngineEvent::Shutdown => {
                    self.status = "mpv 已退出".into();
                }
                _ => {}
            }
        }
        if self.duration > 0.0 && self.last_progress_save.elapsed() >= PROGRESS_SAVE_INTERVAL {
            self.set_phase(PH_SAVE_PROGRESS);
            self.last_progress_save = Instant::now();
            self.save_progress();
            self.save_danmaku_binding();
        }
        if self.last_stats_sample.elapsed() >= STATS_SAMPLE_INTERVAL {
            self.set_phase(PH_PROP_FORMAT);
            self.last_stats_sample = Instant::now();
            self.refresh_decode_stats(&client);
        }
        // 远程弹幕/字幕后台预取结果回流（非阻塞）。
        self.poll_remote_prefetch();
    }

    /// 冻结取证：更新当前阶段标记（watchdog 停滞触发时读取）。
    fn set_phase(&self, phase: u8) {
        if let Some(probe) = &self.probe {
            probe
                .phase
                .store(phase, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 刷新解码统计：编码格式 / 软硬解 / VO 丢帧数 / 视频帧率。
    fn refresh_decode_stats(&mut self, client: &MpvClient) {
        self.set_phase(PH_PROP_FORMAT);
        self.codec = client
            .get_property_string("video-format")
            .unwrap_or_default();
        self.set_phase(PH_PROP_HWDEC);
        self.hwdec = client
            .get_property_string("hwdec-current")
            .unwrap_or_default();
        self.set_phase(PH_PROP_DROPS);
        self.vo_drops = client.get_property_i64("vo-drop-frame-count").unwrap_or(0);
        self.video_fps = client
            .get_property_f64("container-fps")
            .unwrap_or(0.0)
            .max(0.0);
    }

    /// 播放诊断 dump（FileLoaded / Reconfig 各一次，须在 refresh_decode_stats 后调）。
    /// 定案分叉：
    /// - 解码失败 vs 带宽饥饿：`vtracks/sel/fmt/hwdec`（视频轨禁用/软解）
    ///   配合 tick 的 `up=MB/s cache=s`（上游速率与缓冲水位）交叉判定；
    /// - HDR 取证：`prim/tr/sig/levels`（色彩元数据 + 有限全范围）。
    fn log_playback_diag(&self, client: &MpvClient, tag: &str) {
        let tracks = client.get_track_list().unwrap_or_default();
        let vtracks = tracks
            .iter()
            .filter(|t| t.kind == "video")
            .count();
        let sel = tracks
            .iter()
            .filter(|t| t.kind == "video" && t.selected)
            .count();
        let vp = |k: &str| {
            client
                .get_property_string(&format!("video-params/{k}"))
                .unwrap_or_default()
        };
        let sig_peak = client
            .get_property_f64("video-params/sig-peak")
            .unwrap_or(0.0);
        tracing::info!(
            "diag[{}] vtracks={} sel={} fmt={} hwdec={} fps={:.2} {}x{} px={} levels={} prim={} gamma={} sig={:.2} tm={}",
            tag,
            vtracks,
            sel,
            self.codec,
            self.hwdec,
            self.video_fps,
            vp("w"),
            vp("h"),
            vp("pixelformat"),
            vp("colorlevels"),
            vp("primaries"),
            vp("gamma"),
            sig_peak,
            client
                .get_property_string("tone-mapping")
                .unwrap_or_default(),
        );
    }

    fn on_end_file(&mut self, reason: dyplayer_mpv::EndFileReason, error: Option<String>) {
        use dyplayer_mpv::EndFileReason as R;
        // EOF/错误/停止：冻结时钟外推（R04）。auto_next load 后 core-idle
        // 事件变 false 解冻；否则保持冻结在最后位置。
        self.clock.set_stalled(true);
        match reason {
            R::Eof => {
                if let Some(item) = self.current_item() {
                    let path = item.path.to_string_lossy().into_owned();
                    let _ = self.store.mark_finished(self.source_id, &path);
                }
                if self.config.auto_next && self.advance(1) {
                    self.status = "自动连播下一集".into();
                } else {
                    self.status = "播放结束".into();
                }
            }
            R::Error => {
                // 加载/播放失败必须留日志痕（mpv error -13 等只上 UI 状态栏，
                // 无日志则无从取证——error -13 排查实锤过的盲点）。
                let detail = error.clone().unwrap_or_default();
                let file = self
                    .current_item()
                    .map(|i| i.name.clone())
                    .unwrap_or_default();
                tracing::warn!("播放错误 {detail}（文件: {file}）");
                self.status = format!("播放错误 {detail}");
            }
            _ => {
                self.status = format!("停止播放 ({reason:?})");
            }
        }
    }

    /// 远程源：后台线程下载弹幕 XML 与候选字幕，避免 UI 线程同步网络 IO。
    ///
    /// 结果经 channel 回流（`poll_remote_prefetch`）；绑定查询与 cache 目录解析
    /// 是本地操作，留在 UI 线程。源不可用时清空弹幕（与旧行为一致）。
    fn spawn_remote_prefetch(&mut self, video_path: &Path) {
        self.prefetch_danmaku_overridden = false;
        let Some(vfs) = self.vfs.clone() else {
            self.danmaku.on_media_change();
            return;
        };
        let source_id = self.source_id;
        let video_key = video_path.to_string_lossy().into_owned();
        // 绑定查询走本地 SQLite（毫秒级），无需进后台线程。
        let preferred = self
            .store
            .load_bound_danmaku_path(source_id, &video_key)
            .ok()
            .flatten()
            .map(PathBuf::from);
        let sibling = crate::danmaku_ui::xml_path_for_video(video_path);
        let candidates = self
            .current_item()
            .map(|i| i.subs.clone())
            .unwrap_or_default();
        let cache_dir = crate::subtitles::subtitle_cache_dir().ok();
        let video_path = video_path.to_path_buf();
        let ctx = self.egui_ctx.clone();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("dyplayer-remote-prefetch".into())
            .spawn(move || {
                let danmaku = fetch_remote_danmaku(&*vfs, preferred, sibling);
                let subs = fetch_remote_subs(&*vfs, &candidates, cache_dir.as_deref());
                let _ = tx.send(RemotePrefetch {
                    source_id,
                    video_path,
                    danmaku,
                    subs,
                });
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.remote_prefetch = Some(rx),
            Err(e) => tracing::warn!("远程弹幕/字幕预取线程创建失败: {e}"),
        }
    }

    /// 消费远程预取结果（非阻塞；每帧在 poll_events 末尾调用）。
    fn poll_remote_prefetch(&mut self) {
        if self.remote_prefetch.is_none() {
            return;
        }
        let result = self.remote_prefetch.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(res)) => {
                self.remote_prefetch = None;
                self.apply_remote_prefetch(res);
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.remote_prefetch = None;
            }
            _ => {}
        }
    }

    /// 应用后台预取结果：先校验仍是当前条目（换集/换源后的过期结果直接丢弃），
    /// 再挂载弹幕与字幕。
    fn apply_remote_prefetch(&mut self, res: RemotePrefetch) {
        if res.source_id != self.source_id {
            return;
        }
        let Some(cur) = self.current_item().map(|i| i.path.clone()) else {
            return;
        };
        if cur != res.video_path {
            return;
        }
        if let Some((used_path, bytes)) = res.danmaku {
            // 显式选择优先：拖入 XML 已应用时，不被晚到的预取结果覆盖。
            if !self.prefetch_danmaku_overridden {
                let xml = String::from_utf8_lossy(&bytes).into_owned();
                if self.gpu_wid {
                    // R12-C1：GPU 路径跳过主实例解析（覆盖层处理）。
                    self.danmaku_track =
                        Some((res.video_path.clone(), used_path.clone()));
                    let video_key = res.video_path.to_string_lossy().into_owned();
                    self.restore_danmaku_offset(&video_key, &used_path);
                    self.overlay_load(crate::overlay_window::OverlayLoad::Content(xml));
                } else {
                    match self.danmaku.load_xml_content(&xml) {
                        Ok(count) => {
                            self.status =
                                format!("{} · 弹幕 {count} 条", self.status);
                            self.danmaku_track =
                                Some((res.video_path.clone(), used_path.clone()));
                            let video_key =
                                res.video_path.to_string_lossy().into_owned();
                            self.restore_danmaku_offset(&video_key, &used_path);
                            self.overlay_load(
                                crate::overlay_window::OverlayLoad::Content(xml),
                            );
                        }
                        Err(e) => {
                            tracing::warn!("远程弹幕加载失败: {e}");
                            self.danmaku.on_media_change();
                            self.overlay_load(crate::overlay_window::OverlayLoad::Clear);
                        }
                    }
                }
            }
        } else if !self.prefetch_danmaku_overridden {
            self.danmaku.on_media_change();
            self.overlay_load(crate::overlay_window::OverlayLoad::Clear);
        }
        self.prefetch_danmaku_overridden = false;
        self.apply_remote_subs(&res.subs);
    }

    /// 挂载后台预取好的远程字幕（select 语义与本地一致：首个精确匹配选中）。
    fn apply_remote_subs(&mut self, subs: &[PreparedSub]) {
        if subs.is_empty() {
            return;
        }
        let mut first_exact_seen = false;
        for s in subs {
            let select = s.exact && !first_exact_seen;
            if s.exact {
                first_exact_seen = true;
            }
            let path_str = s.prepared.to_string_lossy().into_owned();
            if let Some(client) = &self.client
                && let Err(e) = client.sub_add(&path_str, select, &s.title)
            {
                tracing::warn!("sub-add {} failed: {e}", s.title);
            }
        }
        if !first_exact_seen {
            self.status = format!("{} · 挂载 {} 个候选字幕", self.status, subs.len());
        }
    }

    /// 本地源：挂载同目录候选字幕（本地磁盘读取，无网络挂起风险）。
    /// 远程源的字幕改由后台预取（见 `spawn_remote_prefetch`）。
    fn load_local_subs(&mut self) {
        let Some(item) = self.current_item() else {
            return;
        };
        if item.subs.is_empty() {
            return;
        }
        let cache_dir = match crate::subtitles::subtitle_cache_dir() {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!("subtitle cache dir unavailable: {e}");
                return;
            }
        };
        let mut first_exact_seen = false;
        for cand in &item.subs {
            let prepared = match crate::subtitles::prepare_subtitle(&cand.path, &cache_dir) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "{}",
                        crate::subtitles::describe_prepare_failure(&cand.path, &e)
                    );
                    continue;
                }
            };
            let select = cand.exact && !first_exact_seen;
            if cand.exact {
                first_exact_seen = true;
            }
            let path_str = prepared.to_string_lossy().into_owned();
            if let Some(client) = &self.client
                && let Err(e) = client.sub_add(&path_str, select, &cand.title)
            {
                tracing::warn!("sub-add {} failed: {e}", cand.title);
            }
        }
        if !first_exact_seen && !item.subs.is_empty() {
            self.status = format!("{} · 挂载 {} 个候选字幕", self.status, item.subs.len());
        }
    }

    fn try_resume(&mut self) {
        let Some(item) = self.current_item() else {
            return;
        };
        let path = item.path.to_string_lossy().into_owned();
        let Ok(Some(record)) = self.store.load_play_position(self.source_id, &path) else {
            return;
        };
        if record.finished || record.position_sec < RESUME_MIN_POSITION {
            return;
        }
        if record.duration_sec > 0.0 && record.position_sec >= record.duration_sec * 0.98 {
            return;
        }
        let t = record.position_sec;
        let arg = format!("{t:.3}");
        self.send_command(&["seek", &arg, "absolute"]);
        self.clock.calibrate(t);
        self.status = format!("从 {} 续播", format_time(t));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.set_phase(PH_UI_START);
        self.maybe_enter_fullscreen(frame, ui.ctx());
        self.maybe_attach_host(frame);
        self.handle_shortcuts(ui.ctx());
        self.ui_tick += 1;
        if self.last_tick_log.elapsed() >= Duration::from_secs(1) {
            self.last_tick_log = Instant::now();
            let phase = self
                .probe
                .as_ref()
                .map(|p| p.phase.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            // 诊断差分：上游吞吐速率（MB/s）与本期 GET 数（seek 重连信号）。
            // up 持续低于视频码率 → 带宽饥饿；up 充足 + rc=0 → 解码失败。
            let now = Instant::now();
            let dt = now
                .duration_since(self.diag_last_rate_tick)
                .as_secs_f64()
                .max(0.05);
            let (up_bytes, gets) = self
                .proxy
                .as_ref()
                .map(|p| (p.stats().upstream_bytes(), p.stats().gets()))
                .unwrap_or((0, 0));
            let up_rate = up_bytes.saturating_sub(self.diag_last_upstream) as f64 / 1e6 / dt;
            let gets_delta = gets.saturating_sub(self.diag_last_gets);
            self.diag_last_upstream = up_bytes;
            self.diag_last_gets = gets;
            self.diag_last_rate_tick = now;
            let overlay_stat = self.overlay.as_ref().and_then(|ov| {
                ov.bridge()
                    .lock()
                    .ok()
                    .map(|b| (b.frame_counter, b.visible_count, b.item_count, b.fps))
            });
            tracing::debug!(
                "tick ui={} fs={} paused={} t={:.1} rc={} osc={} dm={}/{} cache={:.1}s up={:.2}MB/s gets=+{} ph={} ov={}",
                self.ui_tick,
                self.fullscreen,
                self.paused,
                self.clock.now(),
                match &self.sw_thread {
                    Some(t) => t.render_calls().load(std::sync::atomic::Ordering::Relaxed),
                    None => self.render_calls.load(std::sync::atomic::Ordering::Relaxed),
                },
                self.osc_ui.shown,
                self.danmaku.last_visible,
                self.danmaku.item_count(),
                self.cache_time,
                up_rate,
                gets_delta,
                phase_name(phase),
                match overlay_stat {
                    // GPU 覆盖层：帧计数/可见弹幕/总条数/覆盖层 fps（None = SW）
                    Some((frames, vis, items, fps)) => {
                        format!("f{frames}/v{vis}/n{items}/{fps:.0}fps")
                    }
                    None => "-".into(),
                }
            );
        }
        // 帧率采样（0.5s 窗口）：self.fps = UI 重绘率（画面/OSC/弹幕共用管线）；
        // dm_fps = 弹幕层有可见内容提交的帧率（评估弹幕流畅度；上一帧的
        // last_visible 已在本帧开头可得，差一帧对统计无影响）。
        self.frame_counter += 1;
        if self.danmaku.last_visible > 0 {
            self.dm_frame_counter += 1;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_fps_sample).as_secs_f64();
        if elapsed >= 0.5 {
            self.fps = self.frame_counter as f64 / elapsed;
            self.frame_counter = 0;
            self.dm_fps = self.dm_frame_counter as f64 / elapsed;
            self.dm_frame_counter = 0;
            self.last_fps_sample = now;
        }

        let media_time = self.clock.now();


        self.set_phase(PH_UI_CENTRAL);
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                let avail_full = ui.available_rect_before_wrap();
                // GPU wid 路径（批 2 后）：OSC/弹幕/HUD 全部迁入覆盖层悬浮
                // 于视频之上（复刻 SW 形态）——视频**恒铺满** avail_full，
                // 任何弹层（弹幕设置卡片/轨道菜单）都只是悬浮其上，**不做
                // 让位收缩**（用户实锤：右键弹幕面板挤压视频画面不可接受；
                // 卡片本身是半透明深色浮层语义，与 OSC 同族）。弹幕设置
                // 打开期间覆盖层整体隐藏（modal_open，防 owned 层盖卡片），
                // 卡片直接浮在视频上。设置/队列面板是布局级 Panel，avail
                // 自动收缩（egui 布局让位，非视频缩放）。
                let avail = avail_full;
                let video_rect = fit_video_rect(avail, self.video_size);

                // GPU wid 路径：视频由 mpv 宿主子窗口承载（盖在 egui 之上），
                // egui 侧只对齐宿主几何（物理像素，变化才提交）；mpv 自行
                // 处理 aspect letterbox。加载中（video_size=None）也对齐——
                // fit 退化为整块 avail，宿主黑底先行盖住背景。
                // 覆盖层（M2）每帧同步：媒体时间/暂停/即时项 + 屏幕矩形
                // （弹幕铺满 avail_full，不随 OSC 让位收缩——viewport 恒定
                // 避免显隐触发 danmaku set_params 重排，与 SW 语义一致）。
                if self.gpu_wid {
                    let ppp = ui.ctx().pixels_per_point();
                    let rect_px = crate::wid_host::to_px_rect(video_rect, ppp);
                    if let Some(host) = &mut self.host {
                        host.align(rect_px);
                    }
                    self.overlay_sync_frame(media_time, avail_full, ppp);
                    // 宿主鼠标换算偏移（宿主客户原点在覆盖层逻辑系）+
                    // 黑边区 pointer 事件镜像（视频区鼠标由宿主 wnd_proc
                    // 直接入覆盖层队列——DComp alpha=0 穿透，宿主是真实
                    // 命中窗口；黑边区鼠标直达主窗口 egui，在此镜像）。
                    let origin = avail_full.min.to_vec2();
                    let host_offset = video_rect.min - avail_full.min;
                    let mirrored: Vec<egui::Event> = ui.ctx().input(|i| {
                        let evs: Vec<egui::Event> = i
                            .events
                            .iter()
                            .filter_map(|e| match *e {
                                egui::Event::PointerMoved(p) => {
                                    Some(egui::Event::PointerMoved(p - origin))
                                }
                                egui::Event::PointerButton {
                                    pos,
                                    button,
                                    pressed,
                                    modifiers,
                                } => Some(egui::Event::PointerButton {
                                    pos: pos - origin,
                                    button,
                                    pressed,
                                    modifiers,
                                }),
                                // R19：滚轮事件镜像到覆盖层（列表滚动）。
                                egui::Event::MouseWheel {
                                    unit,
                                    delta,
                                    phase,
                                    source,
                                    modifiers,
                                } => Some(egui::Event::MouseWheel {
                                    unit,
                                    delta,
                                    phase,
                                    source,
                                    modifiers,
                                }),
                                _ => None,
                            })
                            .collect();
                        evs
                    });
                    if !mirrored.is_empty()
                        && let Some(ov) = &self.overlay
                        && let Ok(mut b) = ov.bridge().lock()
                    {
                        b.mouse_events.extend(mirrored);
                        b.host_offset = host_offset;
                    } else if let Some(ov) = &self.overlay
                        && let Ok(mut b) = ov.bridge().lock()
                    {
                        b.host_offset = host_offset;
                    }
                }

                if self.fullscreen && !self.fs_geom_logged {
                    self.fs_geom_logged = true;
                    tracing::info!(
                        "FS_GEOM: avail={avail:?} video_rect={video_rect:?} video_size={:?} ppp={:.3}",
                        self.video_size,
                        ui.ctx().pixels_per_point()
                    );
                }
                if let Some(_src_size) = self.video_size {
                    if !self.gpu_wid {
                        #[cfg(feature = "backend-wgpu")]
                        {
                            // SW 渲染线程：render_sw + upload 在独立线程，主线程只画。
                            let sw = sw_render_size(video_rect, ui.ctx().pixels_per_point());
                            if let Some(t) = &self.sw_thread {
                                let mut b = t
                                    .bridge()
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                b.set_sw_size(sw);
                                b.set_paused(self.paused);
                                if let Some(tex_id) = b.front_tex_id() {
                                    ui.painter().image(
                                        tex_id,
                                        video_rect,
                                        egui::Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        egui::Color32::WHITE,
                                    );
                                }
                            }
                        }
                        #[cfg(not(feature = "backend-wgpu"))]
                        {
                            // glow GL 互操作：mpv 渲染到离屏 FBO，egui 采样同一纹理。
                            // PaintCallback 在 egui paint 线程执行——不在 ui() 里做
                            // pacing 预判（曾因 delta>3ms 不挂 PaintCallback 致 paint
                            // 线程永不 render，request_repaint_after_for 唤醒时机不
                            // 精确错过 render 窗口→无响应）。has_frame 即挂回调，
                            // block=false 不阻塞 paint 线程，重绘靠 update callback
                            // 驱动。render context 未启 ADVANCED_CONTROL（见
                            // MpvRenderContext::create），report_swap 为空操作。
                            let ppp = ui.ctx().pixels_per_point();
                            let size = sw_render_size(video_rect, ppp);
                            let rebuilt = self.video.ensure(frame, size);
                            let render = self.render.clone();
                            if let (Some(render), Some(tex_id)) = (render, self.video.texture_id()) {
                                let flags = render.request_update();
                                let has_frame = rebuilt || (flags & MPV_RENDER_UPDATE_FRAME) != 0;
                                if has_frame
                                    && let (Some(fbo), Some(_vsize)) =
                                        (self.video.framebuffer_id(), self.video_size)
                                {
                                    let target_render = render.clone();
                                    let callback = egui_glow::CallbackFn::new(
                                        move |_info, painter| unsafe {
                                            reset_gl_for_mpv(painter.gl());
                                            if let Err(e) = target_render
                                                .render(fbo, size[0], size[1], false, false)
                                            {
                                                tracing::error!(
                                                    "mpv gl render failed: {e}"
                                                );
                                            } else {
                                                target_render.report_swap();
                                            }
                                        },
                                    );
                                    ui.painter().add(egui::PaintCallback {
                                        rect: video_rect,
                                        callback: std::sync::Arc::new(callback),
                                    });
                                }
                                ui.painter().image(
                                    tex_id,
                                    video_rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            }
                        }
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("waiting for video...").color(egui::Color32::GRAY),
                        );
                    });
                }

                // R12-B1：收取后台 XML 加载结果（SW 路径 on_file_loaded 异步化）。
                self.danmaku.poll_xml_load();
                // 弹幕铺满整个播放区域（窗口锚点，不随视频矩形收缩）。
                // GPU 路径弹幕由覆盖层线程渲染（overlay_sync_frame 同步），
                // 此处仅 SW 路径画。
                self.set_phase(PH_UI_DANMAKU);
                if !self.gpu_wid {
                    self.danmaku.render(ui, avail, media_time);
                }

                // 画布点击/双击语义（唤 OSC / 双击暂停）统一在 osc::draw
                // 的全屏点击层处理（SW 主线程 / GPU 覆盖层同一套代码）。

                // OSC 自绘控制层：SW 在主线程 overlay 绘制（天然浮于视频
                // 纹理之上）；GPU 在覆盖层线程绘制（悬浮于宿主视频之上，
                // 命令经桥回传）。一套代码两处运行（osc_state.rs 契约）。
                self.set_phase(PH_UI_TRANSPORT);
                if !self.gpu_wid {
                    let st = self.osc_state();
                    let mut cmds = Vec::new();
                    osc::draw(&st, &mut self.osc_ui, ui, &mut cmds);
                    drop(st);
                    self.exec_osc_cmds(cmds);
                }
            });

        // GPU 路径：覆盖层 OSC 命令回传消费（ui() 每帧）。
        if self.gpu_wid {
            // 先取数据（锁内仅 clone/drain，不与执行交叠）。
            let drained = self
                .overlay
                .as_ref()
                .and_then(|ov| ov.bridge().lock().ok())
                .map(|mut b| {
                    let cmds: Vec<crate::osc_state::OscCmd> = std::mem::take(&mut b.osc_cmds);
                    (cmds, b.osc_btn_rect.take())
                });
            if let Some((cmds, btn_rect)) = drained {
                if let Some(r) = btn_rect {
                    self.osc_ui.danmaku_btn_rect = Some(r);
                }
                self.exec_osc_cmds(cmds);
            }
        }

        let danmaku_anchor = self
            .osc_ui
            .danmaku_btn_rect
            .map(|r| egui::pos2(r.center().x, r.top() - PANEL_GAP_ABOVE_BAR));
        let osc_theme =
            osc::THEMES[self.osc_theme_idx.min(osc::THEMES.len() - 1)];
        // 弹幕设置弹窗关闭沿：滑条拖动期间仅内存生效，关闭时一次性落盘
        // （on_exit 兜底直关不落弹窗的场景）
        let danmaku_settings_was_open = self.danmaku.settings_open;
        self.danmaku.show_settings(ui, danmaku_anchor, osc_theme);
        if danmaku_settings_was_open && !self.danmaku.settings_open {
            self.save_danmaku_config();
        }
        // 弹窗内手动选择的弹幕文件已加载成功：绑定 (视频, 弹幕文件) 关系并恢复偏移
        if let Some(xml) = self.danmaku.manual_pick.take() {
            self.overlay_load(crate::overlay_window::OverlayLoad::XmlPath(xml.clone()));
            self.bind_danmaku(&xml);
        }

        self.set_phase(PH_UI_END);
        // 暂停/空闲兜底：mpv 不产帧唤醒时保持低频心跳（HUD 隐藏等得以生效）；
        // SW 播放中由 mpv display 配速的 update callback 以屏幕刷新率驱动满帧。
        // GPU wid 路径无 update callback：播放中按屏幕刷新率心跳驱动 eframe
        // 满帧重绘（弹幕/OSC/进度条平滑），暂停时维持 100ms 低频。
        let heartbeat = if self.gpu_wid && !self.paused {
            // 用精确刷新率对齐 vsync（60Hz→16.67ms，120Hz→8.33ms），
            // 检测失败退回 16ms。
            crate::refresh::cached_hz()
                .filter(|hz| hz.is_finite() && *hz > 0.0)
                .map(|hz| Duration::from_micros((1_000_000.0 / hz) as u64))
                .unwrap_or_else(|| Duration::from_millis(16))
        } else {
            Duration::from_millis(100)
        };
        ui.ctx().request_repaint_after(heartbeat);
    }


    /// 将播放器设置写回 config.toml（与其他配置段合并保存）。
    fn save_player_config(&self) {
        let Ok(dir) = dyplayer_store::AppConfig::config_dir() else {
            return;
        };
        match dyplayer_store::AppConfig::load_or_default() {
            Ok(mut cfg) => {
                cfg.player = self.config.clone();
                if let Err(e) = cfg.save(&dir.join("config.toml")) {
                    tracing::warn!("保存播放器配置失败: {e}");
                }
            }
            Err(e) => tracing::warn!("读取配置失败，无法保存播放器设置: {e}"),
        }
    }


    /// 幂等关闭：停渲染线程 → 释放 render context → quit mpv + join 事件
    /// 线程 → 释放 client → 销毁宿主/覆盖窗口。正常退出与会话重建共用
    /// （R01：重建原直接 drop 致事件线程长期存活）。
    ///
    /// F4：SW 渲染线程 1s 内未退出时，若按原顺序 `render=None` → quit →
    /// `client=None`（terminate_destroy），worker 后退出时 `render_sw` 触
    /// 已死 core = UB。超时分支把 client/event_thread/render 移交 reaper
    /// 线程，reaper 顺序：join worker → drop render（free）→ quit/join
    /// event → drop client（terminate_destroy），保证 free 早于 core 销毁。
    fn shutdown_inner(&mut self) {
        let detached_worker = self.sw_thread.take().and_then(|sw| sw.shutdown());

        if let Some(worker) = detached_worker {
            // 超时路径：移交 reaper，UI 不冻结，释放顺序正确。
            let render = self.render.take();
            let client = self.client.take();
            let event_thread = self.event_thread.take();
            let spawned = std::thread::Builder::new()
                .name("dyplayer-session-reaper".into())
                .spawn(move || reap_session(worker, render, client, event_thread))
                .is_ok();
            if !spawned {
                // spawn 失败通常意味着系统资源耗尽、进程即将退出；
                // 此时 worker/render/client 已 move 进闭包随其 drop，
                // 顺序不保证，但系统已濒死，仅记 error。
                tracing::error!("reaper spawn 失败，会话释放顺序不保证");
            }
        } else {
            // 正常路径：worker 已 join 退出，render 可立即 free（worker 已
            // drop 它的 Arc 克隆，主线程是最后一份）。
            self.render = None;
            if let Some(client) = self.client.as_ref() {
                let _ = client.command(&["quit"]);
            }
            if let Some(handle) = self.event_thread.take() {
                let _ = handle.join();
            }
            self.client = None;
        }
        // 宿主窗口：此刻 mpv 已死（正常路径）或交由 reaper 处理（超时路径），
        // DestroyWindow 不再触碰 vo。
        self.host = None;
        // 覆盖窗口（独立线程 wgpu）随后收线程；此刻 mpv/宿主已死，覆盖层
        // 只是纯透明窗，join 60fps 自续循环单帧内必退出。
        self.overlay = None;
    }

    pub fn on_exit(&mut self) {
        self.save_progress();
        // 弹幕偏移（对级）与全局设置 + 音量（config.toml）收口
        self.save_danmaku_binding();
        // R20B：GPU 路径下覆盖层设置卡片可能未关闭——先请求覆盖线程
        // 导出最终配置，等一帧（≤20ms）后读取，避免保存旧配置。
        if let Some(ov) = &self.overlay {
            if let Ok(mut b) = ov.bridge().lock() {
                b.config_flush_request = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Some(cfg) = ov.bridge().lock().ok().and_then(|mut b| b.config_flush_result.take()) {
                self.danmaku.apply_config(&cfg);
            }
        }
        self.save_danmaku_config();
        self.save_player_config();
        self.shutdown_inner();
    }
}

impl Drop for PlayerSession {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// F4 reaper：SW 渲染线程超时未退出时，在独立线程中按 mpv 契约顺序
/// 释放会话资源：先 join worker（它持 `Arc<MpvRenderContext>`，退出后
/// drop 该 Arc）→ drop render（若主线程已 take，此处是最后一份 →
/// `mpv_render_context_free`）→ quit + join 事件线程 → drop client
/// （`mpv_terminate_destroy`）。free 始终早于 core 销毁。
fn reap_session(
    worker: std::thread::JoinHandle<()>,
    render: Option<std::sync::Arc<MpvRenderContext>>,
    client: Option<std::sync::Arc<MpvClient>>,
    event_thread: Option<std::thread::JoinHandle<()>>,
) {
    let _ = worker.join();
    drop(render);
    if let Some(client) = &client {
        let _ = client.command(&["quit"]);
    }
    if let Some(handle) = event_thread {
        let _ = handle.join();
    }
    drop(client);
}


/// 将启动视图偏好写回 config.toml 的 [ui] 段（读改写，保留其他配置）。
fn save_startup_view(view: dyplayer_store::StartupView) {
    let Ok(dir) = dyplayer_store::AppConfig::config_dir() else {
        return;
    };
    match dyplayer_store::AppConfig::load_or_default() {
        Ok(mut cfg) => {
            cfg.ui.startup_view = view;
            if let Err(e) = cfg.save(&dir.join("config.toml")) {
                tracing::warn!("保存启动视图设置失败: {e}");
            }
        }
        Err(e) => tracing::warn!("读取配置失败，无法保存启动视图设置: {e}"),
    }
}

/// 将全局明暗主题偏好写回 config.toml 的 [ui] 段（读改写，保留其他配置）。
fn save_theme(mode: dyplayer_store::ThemeMode) {
    let Ok(dir) = dyplayer_store::AppConfig::config_dir() else {
        return;
    };
    match dyplayer_store::AppConfig::load_or_default() {
        Ok(mut cfg) => {
            cfg.ui.theme = mode;
            if let Err(e) = cfg.save(&dir.join("config.toml")) {
                tracing::warn!("保存主题设置失败: {e}");
            }
        }
        Err(e) => tracing::warn!("读取配置失败，无法保存主题设置: {e}"),
    }
}

fn video_size_from(dw: i64, dh: i64, w: i64, h: i64) -> Option<[i32; 2]> {
    if dw > 0 && dh > 0 {
        return Some([dw as i32, dh as i32]);
    }
    if w > 0 && h > 0 {
        return Some([w as i32, h as i32]);
    }
    None
}

fn read_video_size(client: &dyplayer_mpv::MpvClient) -> Option<[i32; 2]> {
    let dw = client.get_property_i64("dwidth").unwrap_or(0).max(0);
    let dh = client.get_property_i64("dheight").unwrap_or(0).max(0);
    let w = client.get_property_i64("width").unwrap_or(0).max(0);
    let h = client.get_property_i64("height").unwrap_or(0).max(0);
    video_size_from(dw, dh, w, h)
}

fn fit_video_rect(avail: egui::Rect, video_size: Option<[i32; 2]>) -> egui::Rect {
    let Some([w, h]) = video_size else {
        return avail;
    };
    let (w, h) = (w as f32, h as f32);
    if w <= 0.0 || h <= 0.0 {
        return avail;
    }
    let aspect = w / h;
    let mut size = avail.size();
    if size.x / size.y > aspect {
        size.x = size.y * aspect;
    } else {
        size.y = size.x / aspect;
    }
    egui::Rect::from_center_size(avail.center(), size)
}

/// SW 渲染目标尺寸：显示矩形（逻辑坐标）× DPI 缩放，四舍五入并 clamp 到 ≥1。
fn sw_render_size(video_rect: egui::Rect, pixels_per_point: f32) -> [i32; 2] {
    let w = (video_rect.width() * pixels_per_point).round().max(1.0) as i32;
    let h = (video_rect.height() * pixels_per_point).round().max(1.0) as i32;
    [w.max(1), h.max(1)]
}

/// 把 GL 状态恢复成 libmpv 渲染 API 约定的 OpenGL 默认值。
///
/// render_gl.h：mpv 只豁免 glViewport / glScissor / glBlendFuncSeparate /
/// glClearColor，其余状态都按 OpenGL 默认值假设。而本回调在 egui 绘制中途
/// 执行，egui 留下的 GL_BLEND 开启（预乘 alpha 混合）会让 mpv 写入 FBO 时
/// alpha<255 的像素被混合成黑块/横纹，因此必须逐帧复位。
#[cfg(not(feature = "backend-wgpu"))]
unsafe fn reset_gl_for_mpv(gl: &glow::Context) {
    unsafe {
        for cap in [
            glow::SCISSOR_TEST,
            glow::BLEND,
            glow::CULL_FACE,
            glow::DEPTH_TEST,
            glow::STENCIL_TEST,
            glow::FRAMEBUFFER_SRGB,
            glow::POLYGON_OFFSET_FILL,
            glow::SAMPLE_ALPHA_TO_COVERAGE,
            glow::SAMPLE_COVERAGE,
            glow::DITHER,
        ] {
            gl.disable(cap);
        }
        gl.color_mask(true, true, true, true);
        gl.depth_mask(true);
        for target in [
            glow::PIXEL_UNPACK_BUFFER,
            glow::PIXEL_PACK_BUFFER,
            glow::ARRAY_BUFFER,
            glow::ELEMENT_ARRAY_BUFFER,
            glow::UNIFORM_BUFFER,
            glow::SHADER_STORAGE_BUFFER,
        ] {
            gl.bind_buffer(target, None);
        }
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
        gl.pixel_store_i32(glow::UNPACK_SWAP_BYTES, 0);
        gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::PACK_SKIP_ROWS, 0);
        gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, 0);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_vertex_array(None);
        gl.use_program(None);
    }
}

/// 轨道绑定值 → 应用计划。
/// - `bound == None`：无记录，不干预（保持 apply_lang_prefs 的 auto/no）。
/// - `bound == Some(0)`：上次显式关闭 → set "no"。
/// - `bound == Some(id)` 且 id 在当前 track-list → set id；
///   不在（文件版本/轨数变化）→ 放弃，保持语言偏好行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackApply {
    NoChange,
    Set(Option<i64>),
}

fn track_apply_plan(bound: Option<i64>, available_ids: &[i64]) -> TrackApply {
    match bound {
        None => TrackApply::NoChange,
        Some(0) => TrackApply::Set(None),
        Some(id) if available_ids.contains(&id) => TrackApply::Set(Some(id)),
        Some(_) => TrackApply::NoChange,
    }
}

/// 画质档位预置（媒体库设置三选一，会话重建型）。
///
/// 控制 mpv 缩放/插帧参数：高档零回归（mpv 默认 gpu-hq profile），
/// 低功耗档降级采样器 + 关 sigmoid/interpolation，避免弱 iGPU（如 N2940
/// Gen7 4EU）GPU 反压解码管线。参数在用户 extra_args 之前注入，用户可覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualityPreset {
    /// 高（默认）：mpv 默认 gpu-hq profile（lanczos + sigmoid + interpolation），零回归。
    High,
    /// 平衡：spline36 采样器 + interpolation=auto，中端 GPU 降一档保 sigmoid。
    Balanced,
    /// 低功耗：bilinear + 关 sigmoid/interpolation/correct-downscaling，弱 iGPU 专用。
    LowPower,
}

impl QualityPreset {
    /// 渲染顺序（高 / 平衡 / 低功耗）。
    pub(crate) const ALL: [QualityPreset; 3] =
        [QualityPreset::High, QualityPreset::Balanced, QualityPreset::LowPower];

    /// 中文标签。
    pub(crate) fn label(self) -> &'static str {
        match self {
            QualityPreset::High => "高",
            QualityPreset::Balanced => "平衡",
            QualityPreset::LowPower => "低功耗",
        }
    }

    /// 持久化字符串（config.toml quality_preset 字段）。
    pub(crate) fn config_value(self) -> &'static str {
        match self {
            QualityPreset::High => "high",
            QualityPreset::Balanced => "balanced",
            QualityPreset::LowPower => "low-power",
        }
    }

    /// 由 config 字符串判定预置；未知/空值归入高（零回归兜底）。
    pub(crate) fn from_config(value: &str) -> QualityPreset {
        match value.trim().to_ascii_lowercase().as_str() {
            "balanced" => QualityPreset::Balanced,
            "low-power" | "lowpower" | "low" => QualityPreset::LowPower,
            _ => QualityPreset::High,
        }
    }

    /// 对应的 mpv 选项（name, value）列表；高档返回空（用 mpv 默认）。
    /// 在用户 extra_args 之前注入，用户可覆盖。
    pub(crate) fn mpv_options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            QualityPreset::High => &[],
            QualityPreset::Balanced => &[
                ("scale", "spline36"),
                ("cscale", "spline36"),
                ("dscale", "spline36"),
                ("interpolation", "auto"),
            ],
            QualityPreset::LowPower => &[
                ("scale", "bilinear"),
                ("cscale", "bilinear"),
                ("dscale", "bilinear"),
                ("sigmoid-upscaling", "no"),
                ("interpolation", "no"),
                ("correct-downscaling", "no"),
            ],
        }
    }
}

/// extra_args 编辑器快捷预设（设置面板"高级参数"行预设按钮）。
///
/// 点击预设后填充 extra_args 多行编辑器，用户可再编辑。与画质档位独立：
/// 画质档位注入在 extra_args 之前，extra_args 可覆盖。预设只是快捷模板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtraArgsPreset {
    /// 默认：清空 extra_args（全靠画质档位 + 代码注入）。
    Default,
    /// 影院：ewa_lanczossharp + sigmoid + interpolation + dither，强 GPU 高质量。
    Cinema,
    /// 平衡：spline36 + interpolation=auto，中端 GPU。
    Balanced,
    /// 低功耗+：bilinear + 关 sigmoid/interp + threads=4 + cache-secs=20，
    /// 弱 iGPU（N2940/Atom 类）+ 软解防 demuxer 饥饿。
    LowPower,
}

impl ExtraArgsPreset {
    /// 渲染顺序（默认 / 影院 / 平衡 / 低功耗）。
    pub(crate) const ALL: [ExtraArgsPreset; 4] = [
        ExtraArgsPreset::Default,
        ExtraArgsPreset::Cinema,
        ExtraArgsPreset::Balanced,
        ExtraArgsPreset::LowPower,
    ];

    /// 中文标签。
    pub(crate) fn label(self) -> &'static str {
        match self {
            ExtraArgsPreset::Default => "默认",
            ExtraArgsPreset::Cinema => "影院",
            ExtraArgsPreset::Balanced => "平衡",
            ExtraArgsPreset::LowPower => "低功耗+",
        }
    }

    /// 对应的 extra_args 多行文本（每行一个 name=value）；默认返回空。
    pub(crate) fn args_text(self) -> &'static str {
        match self {
            ExtraArgsPreset::Default => "",
            ExtraArgsPreset::Cinema => {
                "scale=ewa_lanczossharp\ncscale=ewa_lanczossharp\nsigmoid-upscaling=yes\ninterpolation=yes\ndither-depth=auto\ncorrect-downscaling=yes"
            }
            ExtraArgsPreset::Balanced => {
                "scale=spline36\ncscale=spline36\ninterpolation=auto"
            }
            ExtraArgsPreset::LowPower => {
                "scale=bilinear\ncscale=bilinear\ndscale=bilinear\nsigmoid-upscaling=no\ninterpolation=no\ncorrect-downscaling=no\nvd-lavc-threads=4\ncache-secs=20"
            }
        }
    }
}

/// 解码方式预置（设置面板三选一，调试用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HwdecPreset {
    /// 自动（mpv `auto-safe`：优先硬件解码，仅用已知安全方式）。
    Auto,
    /// 强制硬件解码（mpv `d3d11va`；编码不受硬件支持时解码失败，便于对照）。
    Hardware,
    /// 纯软件解码（mpv `no`）。
    Software,
}

impl HwdecPreset {
    /// 渲染顺序（自动 / 硬解 / 软解）。
    pub(crate) const ALL: [HwdecPreset; 3] = [HwdecPreset::Auto, HwdecPreset::Hardware, HwdecPreset::Software];

    /// 中文标签。
    pub(crate) fn label(self) -> &'static str {
        match self {
            HwdecPreset::Auto => "自动",
            HwdecPreset::Hardware => "硬解",
            HwdecPreset::Software => "软解",
        }
    }

    /// 对应的 mpv `hwdec` 取值。
    pub(crate) fn mpv_value(self) -> &'static str {
        match self {
            HwdecPreset::Auto => "auto-safe",
            HwdecPreset::Hardware => "d3d11va",
            HwdecPreset::Software => "no",
        }
    }

    /// 由当前 mpv `hwdec` 字符串判定预置；未知/空值归入自动。
    pub(crate) fn from_mpv_value(value: &str) -> HwdecPreset {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto-safe" | "auto" => HwdecPreset::Auto,
            "no" => HwdecPreset::Software,
            _ => HwdecPreset::Hardware,
        }
    }
}

/// 解码统计一行：`H264·软解·丢帧3` / `HEVC·硬解(d3d11va)·丢帧0`；未加载视频时为 `N/A`。
fn format_decode_info(codec: &str, hwdec: &str, drops: i64) -> String {
    if codec.is_empty() {
        return "N/A".into();
    }
    let hwdec = if hwdec.is_empty() || hwdec.eq_ignore_ascii_case("no") {
        "软解".to_string()
    } else {
        format!("硬解({hwdec})")
    };
    format!("{}·{}·丢帧{drops}", codec.to_uppercase(), hwdec)
}

pub fn format_time(t: f64) -> String {
    if t < 0.0 || !t.is_finite() {
        return "00:00".into();
    }
    let total = t as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// 内置中文系统字体候选（显示名 → 字体文件）。UI 字体切换用；检测字体
/// CJK 支持需解析 cmap（成本高），按"常见中文字体 + 文件存在"过滤即可。
/// 宋体（simsun.ttc）因 epaint 0.37 color_fonts 误判为彩色字体而排除，
/// 勿回退——选宋体会走彩色字体管线导致渲染异常。
pub const SYSTEM_CJK_FONTS: &[(&str, &str)] = &[
    ("微软雅黑", "msyh.ttc"),
    ("等线", "deng.ttf"),
    ("黑体", "simhei.ttf"),
    ("楷体", "simkai.ttf"),
    ("仿宋", "simfang.ttf"),
];

/// 本机可用的中文系统字体显示名（C:\Windows\Fonts 文件存在过滤）。
pub fn available_system_cjk_fonts() -> Vec<&'static str> {
    SYSTEM_CJK_FONTS
        .iter()
        .filter(|(_, file)| std::path::Path::new("C:\\Windows\\Fonts").join(file).exists())
        .map(|(name, _)| *name)
        .collect()
}

/// 系统字体显示名 → 字体文件路径。
fn system_font_path(display_name: &str) -> Option<std::path::PathBuf> {
    SYSTEM_CJK_FONTS
        .iter()
        .find(|(name, _)| *name == display_name)
        .map(|(_, file)| std::path::Path::new("C:\\Windows\\Fonts").join(file))
}

/// 装载全局字体链：拉丁默认 + CJK 系统回退 + emoji + 图标 PUA + 主 UI 字体。
/// `choice` 决定主 UI 字体（App = HarmonyOS Sans SC；System = 系统中文字体，
/// 加载失败回退 HarmonyOS，再失败纯系统回退链），插入 Proportional /
/// Monospace 链头与 Name("HarmonyOS Sans SC") 链头——全应用（播放页
/// Proportional 控件 + 浏览页 Name family）统一生效。可随时重调切换
/// （ctx.set_fonts 触发全量重绘，低频操作可接受）。
pub fn install_cjk_font(ctx: &egui::Context, choice: &dyplayer_store::FontChoice) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: &[&str] = &[
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(PathBuf::from(path)) {
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(data).into());
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.push("cjk".into());
            }
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.push("cjk".into());
            }
            break;
        }
    }
    let emoji_path = "C:\\Windows\\Fonts\\seguiemj.ttf";
    if let Ok(data) = std::fs::read(PathBuf::from(emoji_path)) {
        let tweak = egui::FontTweak {
            y_offset_factor: 0.2,
            ..Default::default()
        };
        fonts.font_data.insert(
            "emoji".into(),
            egui::FontData::from_owned(data).tweak(tweak).into(),
        );
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.push("emoji".into());
        }
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.push("emoji".into());
        }
    }
    // ModernZ 图标字体（PUA 码点 U+F000..U+F090，仅 OSC 验证窗口使用）。
    // 追加在族列表末尾：常规文本字符先命中前面的字体，不受影响。
    fonts.font_data.insert(
        "modernz-icons".into(),
        egui::FontData::from_static(MODERNZ_ICONS_TTF).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.push("modernz-icons".into());
        }
    }

    // 主 UI 字体：System 优先（找不到/读不了回退 App）。
    let mut primary: Option<String> = match choice {
        dyplayer_store::FontChoice::App => None,
        dyplayer_store::FontChoice::System(name) => system_font_path(name)
            .and_then(|path| std::fs::read(path).ok())
            .map(|data| {
                fonts
                    .font_data
                    .insert("ui-primary".into(), egui::FontData::from_owned(data).into());
                "ui-primary".into()
            }),
    };
    if primary.is_none() {
        // App 选择 / System 回退：HarmonyOS Sans SC Medium（统一中文字体，
        // 方案见 docs/TDD-DyPlayer-FontUnification-20261019011024.md）。
        for path in harmonyos_font_candidates() {
            if let Ok(data) = std::fs::read(&path) {
                fonts
                    .font_data
                    .insert("harmonyos".into(), egui::FontData::from_owned(data).into());
                primary = Some("harmonyos".into());
                break;
            }
        }
    }
    // 主字体插链头：Proportional / Monospace 全应用生效（含拉丁字形，
    // HarmonyOS/微软雅黑等均自带，风格统一）。
    if let Some(key) = &primary {
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.insert(0, key.clone());
            }
        }
    }
    // Name family "HarmonyOS Sans SC"：浏览域（验证页/媒体库）专用链，
    // 链头 = 主字体（App→HarmonyOS；System→系统字体），后接 Proportional
    // 克隆保住 emoji / 图标回退。family 必须无条件注册——未注册的 Name
    // family 在 epaint 0.37 是空回退链，所有字符画 synthetic tofu box。
    let mut chain: Vec<String> = Vec::new();
    if let Some(key) = &primary {
        chain.push(key.clone());
    }
    chain.extend(
        fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts
        .families
        .insert(egui::FontFamily::Name("HarmonyOS Sans SC".into()), chain);
    ctx.set_fonts(fonts);
}

/// HarmonyOS Sans SC Medium 候选路径（任一命中即可）：
/// 1. 编译期 manifest 绝对路径——开发机任意 cwd 可达（含 cargo test，
///    其 cwd 是 crate 目录而非 workspace）；
/// 2. cwd 相对——dyplayer/ 下 cargo run；
/// 3. exe 同目录——分发布局（fonts/ 随包）。
///
/// pub(crate)：弹幕 overlay 的字体选择（App 选项）复用同一路径链。
pub(crate) fn harmonyos_font_candidates() -> Vec<PathBuf> {
    let manifest = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fonts/HarmonyOS_Sans_SC_Medium.ttf"
    ));
    let mut candidates = vec![
        manifest,
        PathBuf::from("fonts").join("HarmonyOS_Sans_SC_Medium.ttf"),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("fonts").join("HarmonyOS_Sans_SC_Medium.ttf"));
    }
    candidates
}

/// ModernZ 图标字体（Fluent MIT / Material Apache-2.0，可嵌入分发）。
const MODERNZ_ICONS_TTF: &[u8] = include_bytes!("../assets/modernz-icons.ttf");
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harmonyos_name_family_resolves_icons_and_cjk() {
        let ctx = egui::Context::default();
        install_cjk_font(&ctx, &dyplayer_store::FontChoice::App);
        ctx.begin_pass(egui::RawInput::default());
        let family = egui::FontFamily::Name("HarmonyOS Sans SC".into());
        ctx.fonts_mut(|f| {
            let chars = f.characters(&family);
            // 回归：字体数据未加载时 Name family 未注册导致空链 →
            // synthetic tofu（全页豆腐）。现在 family 无条件注册，
            // 图标 PUA 码点必须经回退链命中 modernz-icons。
            assert!(
                chars
                    .get(&'\u{F065}')
                    .is_some_and(|names| names.iter().any(|n| n == "modernz-icons")),
                "icon PUA glyph must hit modernz-icons via fallback chain, got {:?}",
                chars.get(&'\u{F065}')
            );
            assert!(chars.contains_key(&'中'), "CJK must resolve");
            assert!(chars.contains_key(&'a'), "latin must resolve");
        });
        // 离线跑 pass 会产出字体纹理 delta；无渲染器上传，显式丢弃
        // （epaint 对未处理的 TexturesDelta 会在 drop 时 panic）。
        let mut output = ctx.end_pass();
        output.textures_delta.clear();
    }

    #[test]
    fn video_size_from_prefers_display_size_over_source_size() {
        assert_eq!(video_size_from(1920, 1080, 540, 960), Some([1920, 1080]));
    }

    #[test]
    fn video_size_from_falls_back_to_source_size() {
        assert_eq!(video_size_from(0, 0, 540, 960), Some([540, 960]));
        assert_eq!(video_size_from(608, 0, 540, 960), Some([540, 960]));
        assert_eq!(video_size_from(0, 1080, 540, 960), Some([540, 960]));
    }

    /// 轨道绑定恢复计划：无记录不干预 / 0=显式关闭 / id 存在才应用 /
    /// id 不在当前 track-list 时放弃（文件轨变化容错）。
    #[test]
    fn track_apply_plan_covers_all_binding_semantics() {
        let ids = [1i64, 3];
        // 无记录：保持 lang prefs 行为。
        assert_eq!(track_apply_plan(None, &ids), TrackApply::NoChange);
        // 显式关闭：set "no"。
        assert_eq!(track_apply_plan(Some(0), &ids), TrackApply::Set(None));
        // id 在 track-list：应用。
        assert_eq!(track_apply_plan(Some(3), &ids), TrackApply::Set(Some(3)));
        // id 不在（文件版本变化）：放弃。
        assert_eq!(track_apply_plan(Some(2), &ids), TrackApply::NoChange);
        // 空 track-list（无该类轨道）：任何具体 id 都放弃，关闭仍可恢复。
        assert_eq!(track_apply_plan(Some(1), &[]), TrackApply::NoChange);
        assert_eq!(track_apply_plan(Some(0), &[]), TrackApply::Set(None));
    }

    #[test]
    fn video_size_from_rejects_partial_and_negative_sizes() {
        assert_eq!(video_size_from(0, 0, 0, 0), None);
        assert_eq!(video_size_from(1920, -1, 0, 0), None);
        assert_eq!(video_size_from(0, 0, -540, 960), None);
        assert_eq!(video_size_from(0, 0, 0, 960), None);
    }

    #[test]
    fn fit_video_rect_uses_video_aspect() {
        let avail = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 900.0));
        let landscape = fit_video_rect(avail, Some([1920, 1080]));
        assert!((landscape.width() / landscape.height() - 16.0 / 9.0).abs() < 1e-3);
        let portrait = fit_video_rect(avail, Some([540, 960]));
        assert!((portrait.width() / portrait.height() - 9.0 / 16.0).abs() < 1e-3);
    }

    #[test]
    fn sw_render_size_scales_by_dpi() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1536.0, 864.0));
        assert_eq!(sw_render_size(rect, 1.25), [1920, 1080]);
        assert_eq!(sw_render_size(rect, 1.0), [1536, 864]);
    }

    #[test]
    fn sw_render_size_downscales_4k_to_display_rect() {
        // 4K 源（AR 16:9）在 1920x1080 可用区域：SW 尺寸 = 显示尺寸而非源尺寸。
        let avail = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1080.0));
        let video_rect = fit_video_rect(avail, Some([3840, 2160]));
        assert_eq!(sw_render_size(video_rect, 1.0), [1920, 1080]);
    }

    #[test]
    fn sw_render_size_clamps_tiny_rect() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.0, 3.2));
        assert_eq!(sw_render_size(rect, 1.0), [1, 3]);
    }

    #[test]
    fn decode_info_marks_software_decoding() {
        assert_eq!(format_decode_info("h264", "no", 3), "H264·软解·丢帧3");
        // mpv 在加载早期可能短暂返回空 hwdec-current，视作软解。
        assert_eq!(format_decode_info("h264", "", 0), "H264·软解·丢帧0");
    }

    #[test]
    fn hwdec_preset_maps_to_mpv_values() {
        assert_eq!(HwdecPreset::Auto.mpv_value(), "auto-safe");
        assert_eq!(HwdecPreset::Hardware.mpv_value(), "d3d11va");
        assert_eq!(HwdecPreset::Software.mpv_value(), "no");
    }

    #[test]
    fn hwdec_preset_classifies_current_value() {
        assert_eq!(HwdecPreset::from_mpv_value("auto-safe"), HwdecPreset::Auto);
        assert_eq!(HwdecPreset::from_mpv_value("auto"), HwdecPreset::Auto);
        assert_eq!(HwdecPreset::from_mpv_value(""), HwdecPreset::Auto);
        assert_eq!(HwdecPreset::from_mpv_value("no"), HwdecPreset::Software);
        assert_eq!(
            HwdecPreset::from_mpv_value("d3d11va"),
            HwdecPreset::Hardware
        );
        assert_eq!(
            HwdecPreset::from_mpv_value("dxva2-copy"),
            HwdecPreset::Hardware
        );
    }

    #[test]
    fn decode_info_marks_hardware_decoding() {
        assert_eq!(
            format_decode_info("hevc", "d3d11va", 0),
            "HEVC·硬解(d3d11va)·丢帧0"
        );
        assert_eq!(
            format_decode_info("av1", "d3d11va-copy", 12),
            "AV1·硬解(d3d11va-copy)·丢帧12"
        );
    }

    #[test]
    fn decode_info_placeholder_before_load() {
        assert_eq!(format_decode_info("", "", 0), "N/A");
    }

    #[test]
    fn panel_toggle_opens_target_and_closes_others() {
        // 设置已开时点"弹幕设置"：只留弹幕设置
        let (q, d, s) = exclusive_panel_state(PlayerPanel::DanmakuSettings, false);
        assert!((q, d, s) == (false, true, false));
        // 队列已开时点"设置"：只留设置
        let (q, d, s) = exclusive_panel_state(PlayerPanel::Settings, false);
        assert!((q, d, s) == (false, false, true));
        // 弹幕设置已开时点"队列"：只留队列
        let (q, d, s) = exclusive_panel_state(PlayerPanel::Queue, false);
        assert!((q, d, s) == (true, false, false));
    }

    #[test]
    fn panel_toggle_on_open_target_closes_all() {
        // 目标面板已开再点一次：三面板全关
        for target in [
            PlayerPanel::Queue,
            PlayerPanel::DanmakuSettings,
            PlayerPanel::Settings,
        ] {
            let (q, d, s) = exclusive_panel_state(target, true);
            assert!((q, d, s) == (false, false, false), "{target:?}");
        }
    }

    /// 测试 helper：跑一个空 pass 并丢弃字体纹理 delta。
    /// egui 0.37 两个坑：(1) fonts 在首次 pass 前惰性不可用（fonts_mut panic）；
    /// (2) set_fonts 后的 pass 会产生 TexturesDelta，未 clear 直接 drop 会
    /// 触发 epaint debug 断言。测试里装完字体必须再跑一个 pass 吸收 delta。
    fn run_empty_pass(ctx: &egui::Context) {
        ctx.begin_pass(egui::RawInput::default());
        let out = ctx.end_pass();
        let mut deltas = out.textures_delta;
        deltas.clear();
    }

    #[test]
    fn app_font_choice_puts_harmonyos_at_chain_head() {
        let ctx = egui::Context::default();
        run_empty_pass(&ctx);
        install_cjk_font(&ctx, &dyplayer_store::FontChoice::App);
        run_empty_pass(&ctx);
        let chain = ctx.fonts_mut(|f| {
            f.definitions()
                .families
                .get(&egui::FontFamily::Proportional)
                .cloned()
                .unwrap_or_default()
        });
        // HarmonyOS 可达（开发机/随包）时必须在链头；不可达时链头不是
        // harmonyos 也允许（回退系统链），只验证链非空。
        if harmonyos_font_candidates().iter().any(|p| p.exists()) {
            assert_eq!(chain.first().map(String::as_str), Some("harmonyos"));
        } else {
            assert!(!chain.is_empty());
        }
    }

    #[test]
    fn system_font_choice_puts_system_font_first() {
        // "黑体"（simhei.ttf）是 Windows 传统内置字体，本机通常存在；
        // 不存在则跳过断言（CI 无中文字体环境）。
        let Some((name, file)) = SYSTEM_CJK_FONTS.iter().find(|(n, _)| *n == "黑体") else {
            return;
        };
        if !std::path::Path::new("C:\\Windows\\Fonts").join(file).exists() {
            return;
        }
        let ctx = egui::Context::default();
        run_empty_pass(&ctx);
        install_cjk_font(&ctx, &dyplayer_store::FontChoice::System(name.to_string()));
        run_empty_pass(&ctx);
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let chain = ctx.fonts_mut(|f| {
                f.definitions()
                    .families
                    .get(&family)
                    .cloned()
                    .unwrap_or_default()
            });
            assert_eq!(chain.first().map(String::as_str), Some("ui-primary"), "{family:?}");
            // 图标回退保留（链尾 modernz-icons 不丢）。
            assert!(
                chain.iter().any(|k| k == "modernz-icons"),
                "{family:?} icon fallback lost"
            );
        }
        let name_chain = ctx.fonts_mut(|f| {
            f.definitions()
                .families
                .get(&egui::FontFamily::Name("HarmonyOS Sans SC".into()))
                .cloned()
                .unwrap_or_default()
        });
        assert_eq!(name_chain.first().map(String::as_str), Some("ui-primary"));
        // fonts_mut 会再次标记字体纹理：末尾再吸收一次 delta，防 Context drop 断言
        run_empty_pass(&ctx);
    }

    #[test]
    fn unknown_system_font_falls_back_to_harmonyos() {
        let ctx = egui::Context::default();
        run_empty_pass(&ctx);
        install_cjk_font(&ctx, &dyplayer_store::FontChoice::System("不存在的字体".into()));
        run_empty_pass(&ctx);
        let chain = ctx.fonts_mut(|f| {
            f.definitions()
                .families
                .get(&egui::FontFamily::Proportional)
                .cloned()
                .unwrap_or_default()
        });
        if harmonyos_font_candidates().iter().any(|p| p.exists()) {
            assert_eq!(chain.first().map(String::as_str), Some("harmonyos"));
        } else {
            assert!(!chain.is_empty());
        }
    }

    /// 内存 VFS：按路径返回预设字节；未登记路径视为不存在（远程预取测试用）。
    struct MemFs {
        files: std::collections::HashMap<PathBuf, Vec<u8>>,
    }

    impl VfsSource for MemFs {
        fn list_dir(&self, _path: &Path) -> dyplayer_vfs::VfsResult<Vec<dyplayer_vfs::VfsEntry>> {
            Ok(Vec::new())
        }
        fn stat(&self, _path: &Path) -> dyplayer_vfs::VfsResult<dyplayer_vfs::VfsMeta> {
            Ok(dyplayer_vfs::VfsMeta {
                size: 0,
                modified: None,
            })
        }
        fn read_full(&self, path: &Path) -> dyplayer_vfs::VfsResult<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| dyplayer_vfs::VfsError::NotFound(path.display().to_string()))
        }
        fn open_range(
            &self,
            path: &Path,
            _start: u64,
            _len: Option<u64>,
        ) -> dyplayer_vfs::VfsResult<dyplayer_vfs::RangeRead> {
            Err(dyplayer_vfs::VfsError::NotFound(path.display().to_string()))
        }
    }

    #[test]
    fn fetch_remote_danmaku_prefers_bound_then_falls_back_to_sibling() {
        let dir = PathBuf::from("/v");
        let sibling = dir.join("a.xml");
        let bound = dir.join("bound.xml");
        let mut files = std::collections::HashMap::new();
        files.insert(sibling.clone(), b"<i><d p=\"1\">s</d></i>".to_vec());
        files.insert(bound.clone(), b"<i><d p=\"2\">b</d></i>".to_vec());
        let fs = MemFs { files };

        // 绑定路径可用：优先绑定，不读同名。
        let got = fetch_remote_danmaku(&fs, Some(bound.clone()), sibling.clone());
        let (path, bytes) = got.expect("绑定路径命中");
        assert_eq!(path, bound);
        assert_eq!(bytes, b"<i><d p=\"2\">b</d></i>");

        // 绑定路径缺失：回退同名 sibling。
        let got = fetch_remote_danmaku(&fs, Some(dir.join("missing.xml")), sibling.clone());
        assert_eq!(got.map(|(p, _)| p), Some(sibling));

        // 两者皆缺失：None（UI 侧据此清空弹幕）。
        assert!(fetch_remote_danmaku(&fs, None, dir.join("nope.xml")).is_none());
    }

    #[test]
    fn fetch_remote_subs_prepares_hits_and_skips_misses() {
        let cache = std::env::temp_dir().join(format!(
            "dyplayer_subs_test_{}_{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();

        let dir = PathBuf::from("/v");
        let ok_path = dir.join("a.zh.srt");
        let miss_path = dir.join("a.jp.srt");
        let mut files = std::collections::HashMap::new();
        files.insert(
            ok_path.clone(),
            "1\n00:00:00,000 --> 00:00:01,000\nhi\n".as_bytes().to_vec(),
        );
        let fs = MemFs { files };
        let candidates = vec![
            SubtitleCandidate {
                path: ok_path.clone(),
                title: "zh".into(),
                exact: true,
                lang: Some("zh".into()),
            },
            SubtitleCandidate {
                path: miss_path.clone(),
                title: "jp".into(),
                exact: false,
                lang: Some("ja".into()),
            },
        ];

        let out = fetch_remote_subs(&fs, &candidates, Some(&cache));
        assert_eq!(out.len(), 1, "仅命中候选应被准备");
        assert_eq!(out[0].title, "zh");
        assert!(out[0].exact);
        assert!(out[0].prepared.exists(), "命中的字幕应落盘缓存");

        // 无缓存目录：直接空结果，不 panic。
        assert!(fetch_remote_subs(&fs, &candidates, None).is_empty());

        let _ = std::fs::remove_dir_all(&cache);
    }
}
