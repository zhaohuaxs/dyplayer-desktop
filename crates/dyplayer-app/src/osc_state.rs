//! OSC 跨线程状态契约（批 2：OSC 迁覆盖层，复刻 SW 浮层形态）。
//!
//! 设计原则：**一套代码两处运行**——osc.rs 的绘制函数只依赖
//! [`OscState`]（只读快照）+ [`OscUiState`]（交互瞬态）+ [`OscCmd`]
//! 命令队列：
//!
//! - SW 路径：主线程组装快照 → osc::draw → 本地执行命令（行为不变）；
//! - GPU 路径：主线程组装快照写桥 → 覆盖线程 osc::draw（悬浮在视频上）
//!   → 命令经桥回传 → 主线程 poll_events 消费执行。
//!
//! OSC 偏好（主题/布局/特效/章节样式）属快照（主线程设置面板是唯一
//! 写入方）；交互瞬态（显隐状态机/seek 预览/菜单开关）属 OscUiState
//! （SW=主线程实例，GPU=覆盖线程实例，各自自治）。

use std::time::Instant;

use dyplayer_mpv::{Chapter, Track};

/// OSC 布局（Default 全功能 / Mini 紧凑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OscLayout {
    Default,
    Mini,
}

/// OSC 渲染所需只读状态快照（主线程每帧组装；GPU 经 OverlayBridge 传递）。
#[derive(Clone)]
pub(crate) struct OscState {
    // ---- 播放核心 ----
    pub title: String,
    pub duration: f64,
    pub position: f64,
    pub paused: bool,
    pub volume: i64,
    pub muted: bool,
    pub speed: f64,
    pub fullscreen: bool,
    pub loop_file: bool,
    /// index > 0（上一集可用）。
    pub can_prev: bool,
    /// index+1 < items.len()（下一集可用）。
    pub can_next: bool,

    // ---- 轨道 / 章节 ----
    pub audio_tracks: Vec<Track>,
    pub sub_tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,

    // ---- 信息块（HUD）----
    pub show_hud: bool,
    pub codec: String,
    pub hwdec: String,
    pub vo_drops: i64,
    pub video_fps: f64,
    pub cache_time: f64,
    pub ui_fps: f64,
    pub dm_fps: f64,

    pub danmaku_enabled: bool,
    pub video_size: Option<[i32; 2]>,
    pub status: String,
    pub queue_pos: usize,
    pub queue_len: usize,
    /// 队列面板开态（卡片迁覆盖层：OSC 同族悬浮卡片）。
    pub queue_open: bool,
    /// 队列标题列表（卡片绘制用；每帧组装）。
    pub queue_titles: Vec<String>,
    /// 设置面板开态（卡片迁覆盖层：OSC 同族悬浮卡片）。
    pub settings_open: bool,
    /// 播放器配置快照（设置卡片绘制用；每帧组装，交互经命令回传）。
    pub player_cfg: dyplayer_store::PlayerConfig,
    /// 引擎 hwdec（设置卡片解码方式选择用）。
    pub engine_hwdec: String,
    /// 渲染路径（只读显示；切换入口在媒体库设置）。
    pub engine_render_path: String,
    /// UI 主题（设置卡片外观选择用）。
    pub ui_theme: dyplayer_store::ThemeMode,
    /// 启动视图（设置卡片"启动时进入播放页"用）。
    pub startup_view: dyplayer_store::StartupView,
    /// 弹幕延迟（会话级；设置卡片 DragValue 用）。
    pub danmaku_delay: f64,

    // ---- 主线程面板开态（OSC keep 判断：面板开时常显）----
    pub panel_open: bool,

    // ---- 键盘 seek 步进（mini 布局按钮 / tips 文案）----
    pub seek_step_back: f64,
    pub seek_step_fwd: f64,

    // ---- OSC 偏好（主线程设置面板唯一写入方）----
    pub theme_idx: usize,
    pub layout: OscLayout,
    pub fx_size: bool,
    pub fx_color: bool,
    pub fx_glow: bool,
    pub nibble: OscNibble,
}

/// OSC 章节标记样式（Gap 进度断开 / Bar 上下条 / Triangle 上三角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OscNibble {
    Gap,
    Bar,
    Triangle,
}

/// OSC 音轨/字幕菜单种类（Foreground 定制卡片弹窗，弹幕设置弹窗同族）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackMenuKind {
    Audio,
    Subtitle,
}

/// OSC 交互瞬态（无 PlayerSession 依赖；SW=主线程实例、GPU=覆盖线程实例）。
pub(crate) struct OscUiState {
    pub shown: bool,
    pub last_activity: Instant,
    pub last_mouse: Option<egui::Pos2>,
    /// 拖动 seekbar 的时间预览（松手前不 seek）。
    pub seek_preview: Option<f64>,
    /// 音轨/字幕菜单开关（卡片渲染在 OSC 所在层）。
    pub track_menu: Option<TrackMenuKind>,
    /// "弹幕"按钮 rect（主线程弹幕设置弹窗锚点：SW 同线程直读 /
    /// GPU 经桥回传）。
    pub danmaku_btn_rect: Option<egui::Rect>,
    /// "播放列表"按钮 rect（队列卡片锚点）。
    pub queue_btn_rect: Option<egui::Rect>,
    /// "设置"按钮 rect（设置卡片锚点）。
    pub settings_btn_rect: Option<egui::Rect>,
}

impl Default for OscUiState {
    fn default() -> Self {
        Self {
            shown: true,
            last_activity: Instant::now(),
            last_mouse: None,
            seek_preview: None,
            track_menu: None,
            danmaku_btn_rect: None,
            queue_btn_rect: None,
            settings_btn_rect: None,
        }
    }
}

/// OSC 交互产生的命令（SW 本地执行 / GPU 经桥回主线程执行）。
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "backend-wgpu"), allow(dead_code))]
pub(crate) enum OscCmd {
    TogglePause,
    SeekTo(f64),
    SeekRel(f64),
    SetVolume(i64),
    ToggleMute,
    SetSpeed(f64),
    ToggleFullscreen,
    ToggleLoopFile,
    TakeScreenshot,
    /// 回媒体库（OSC 媒体库按钮）。
    ExitToLibrary,
    SkipPrev,
    SkipNext,
    /// 信息块开关（config.show_stats_hud + 落盘）。
    ToggleHud,
    SelectAudio(Option<i64>),
    SelectSub(Option<i64>),
    /// rfd 选外部音频文件（主线程弹——覆盖线程不能弹文件对话框）。
    PickAudioFile,
    PickSubFile,
    TogglePanelQueue,
    TogglePanelSettings,
    /// 队列卡片行点击：跳转到指定索引。
    PlayIndex(usize),
    /// 设置卡片：播放器配置变更（主线程 apply diff + save）。
    SetPlayerConfig(dyplayer_store::PlayerConfig),
    /// 设置卡片：解码方式变更（主线程 apply hwdec + save engine）。
    SetHwdec(String),
    /// 设置卡片：主题变更（主线程 save_theme）。
    SetTheme(dyplayer_store::ThemeMode),
    /// 设置卡片：启动视图变更（主线程 save_startup_view）。
    SetStartupView(dyplayer_store::StartupView),
    /// 设置卡片：弹幕延迟变更（会话级）。
    SetDanmakuDelay(f64),
    /// 设置卡片：OSC 偏好变更（主题/布局/章节/特效）。
    SetOscPrefs {
        theme_idx: usize,
        layout: OscLayout,
        nibble: OscNibble,
        fx_glow: bool,
        fx_size: bool,
        fx_color: bool,
    },
    /// 弹幕设置卡片（GPU：覆盖层承载，经桥 toggle；SW：主线程面板）。
    ToggleDanmakuSettings,
    /// 弹幕显示开关（即时项）。
    ToggleDanmakuEnabled,
    /// 弹幕设置卡片关闭沿落盘（覆盖层导出的完整配置；主线程 apply +
    /// 落盘 + push 回环）。
    SaveDanmakuConfig(dyplayer_store::DanmakuConfig),
    /// 弹幕设置卡片"载入弹幕文件…"（主线程 rfd；结果经桥
    /// danmaku_file_result 回覆盖层）。
    PickDanmakuFile,
    /// 弹幕设置卡片"自定义…"字体选择（主线程 rfd；结果经桥
    /// font_file_result 回覆盖层）。
    PickDanmakuFont,
    /// 弹幕设置卡片内手动加载成功的弹幕文件（主线程 bind_danmaku 落
    /// (视频, 弹幕) 绑定 + OverlayLoad::XmlPath 回覆盖层）。
    BindDanmakuFile(std::path::PathBuf),
}