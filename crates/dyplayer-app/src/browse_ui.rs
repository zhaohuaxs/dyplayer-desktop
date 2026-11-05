//! 媒体库 / 浏览主界面（Screen::Browse 的 UI shell）。
//!
//! 卡片化 shell（圆角卡浮在 gutter 上）、集中语义调色板（theme::Palette）、
//! 五导航视图（首页 / 媒体库 / 媒体源 / 最近播放 / 收藏）与全局设置，
//! 自绘导航行 / 富信息媒体行 / 海报墙卡片 / 空状态、hover 动效、封面
//! 几何占位构图；浏览域矢量图标见 glyph.rs。
//!
//! 数据为真实资源库现状（媒体源 / 最近播放 / 收藏 / 当前目录缓存）。
//! 交互经 `ShellAction` 信号帧末由 main.rs 执行（开播 / 跳目录 / 源保存）；
//! ui() 内只读借用 browser/sources，无法直接发起可变操作。
//! 真实可用的交互：视图导航、搜索过滤、主题切换、源选中、开播跳转。
//!
//! UI 约定（docs/DyPlayer-UI最佳实践方案.md）：
//! - Palette + 卡片化 shell（Card + GUTTER + 圆角卡）
//! - 富信息行自带进度条 / 源徽章 / hover 播放钮
//! - hover 动效 / 色块主题切换：animate_value_with_time
//!
//! 隔离性：不触碰 PlayerSession / BrowserState 的状态（只读借用），
//! 唯一写入口是数据节流刷新（recent / favorites / 媒体库缓存）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    Align2, Color32, CornerRadius, FontId, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::browser::{BrowserState, MediaSource, SourceKind};
use crate::glyph::{
    Glyph, fade, mix, paint_glyph, paint_left_center_truncated, source_kind_glyph, truncate_tail,
};
use crate::theme::{Palette, palettes};
use dyplayer_store::{FavoriteEntry, FontChoice, MediaLibEntry, RecentEntry, StartupView, Store};

// ---------------------------------------------------------------------------
// Palette 已全局化至 crate::theme（U0：16 语义色 × 深浅各 4 组），
// 本页仅消费（palettes/palette 取板），设置视图的主题色选择同源。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 字体（HarmonyOS Sans SC Medium，播放页 OSC 同一全局 family）
// ---------------------------------------------------------------------------

const HARMONYOS_FAMILY: &str = "HarmonyOS Sans SC";

pub fn lib_family() -> egui::FontFamily {
    // 与播放页同源的中文字体（HarmonyOS Sans SC）。
    egui::FontFamily::Name(HARMONYOS_FAMILY.into())
}

fn lib_font(size: f32) -> FontId {
    FontId::new(size, lib_family())
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum BrowseTab {
    Home,
    /// 真媒体库：加入的文件夹条目（media_library 表）。
    MediaLib,
    /// 媒体源浏览（原 Library 视图：源目录文件夹卡片）。
    Library,
    Recent,
    Favorites,
    /// 全局设置（外观 / 字体 / 启动视图；写回 config）。
    Settings,
}

pub struct BrowseUiState {
    pub exit_requested: bool,
    view: BrowseTab,
    theme_idx: usize,
    search: String,
    /// 侧栏 / Home 选中的源索引（视觉选中态；Library 视图展示其缓存目录）。
    pub selected_source: Option<usize>,
    /// 待切换的源索引：点击源后置位，由 main.rs 帧末消费
    /// （激活源 + 清缓存 + 请求根目录；ui() 内只读借用 browser，无法直接发起）。
    pub pending_source: Option<usize>,
    /// 编辑/添加源面板（保存经 ShellAction::SaveSource 由 main.rs 帧末落库）。
    edit: Option<EditSourceForm>,
    /// 删除源确认弹窗（右键源行触发；确认经 ShellAction::DeleteSource
    /// 由 main.rs 帧末执行五表级联 + keyring 清理）。
    confirm_delete: Option<(i64, String)>,
    /// 播放记录清除确认弹窗（右键单行 / "清除全部"按钮触发）。
    pub confirm_clear_history: Option<ClearHistoryTarget>,
    /// 弹框内"测试连接"后台任务（弹框关闭即丢弃）。
    source_test: Option<SourceTest>,
    /// 页内提示（操作反馈），短暂显示后淡出。
    notice: Option<(String, Instant)>,
    /// 媒体库条目缓存（media_library 表，进入/增删时刷新）。
    media_lib: Vec<MediaLibEntry>,
    /// 媒体库集数统计缓存（key = media_library.id；后台递归 VFS 回流）。
    pub lib_stats: std::collections::HashMap<i64, EpisodeStats>,
    /// 待统计条目 (lib_id, source_id, folder_path)：渲染发现缺失即入队
    /// （去重），main.rs 帧末批量 spawn 后台线程。
    pub pending_lib_stats: Vec<(i64, i64, String)>,
    /// 上次统计时的播放历史戳（recent 首条 updated_at）：变化 = 有播放/
    /// 进度落库 → 清空 lib_stats 重统计（ensure_data 检测）。
    lib_stats_stamp: Option<String>,
    /// 媒体源视图搜索的索引子目录命中缓存（搜索词/源变化时同步重查，
    /// 帧间直接复用；SQLite LIKE 万行毫秒级，键入触发非每帧）。
    index_hits: Vec<dyplayer_store::IndexHit>,
    index_hits_query: String,
    index_hits_source: i64,
    /// 全局 UI 设置镜像（进入 Browse 页时从 config 同步；设置视图改动经
    /// pending_ui 由 main.rs 帧末写回 config 并落盘）。
    ui_font: FontChoice,
    ui_startup: StartupView,
    /// 渲染路径镜像（true = gpu wid 路径；从 config.engine 同步）。
    /// 同属全局引擎配置：改动经 pending_ui 写回并触发播放会话重建。
    ui_render_gpu: bool,
    /// 画质档位镜像（"high"/"balanced"/"low-power"；从 config.engine 同步）。
    /// 会话重建型：mpv 初始化期 scale/cscale/interpolation 选项。
    ui_quality_preset: String,
    /// 引擎高级参数镜像（多行文本，每行一个 name=value；从 config.engine 同步）。
    /// 会话重建型：mpv 初始化期选项。编辑器失焦/回车时 push pending_ui。
    ui_extra_args: String,
    /// 是否存在硬件 GPU 适配器（App::new 检测 wgpu 枚举结果）。false =
    /// 虚拟机/远程桌面等无硬件 3D 环境：GPU 渲染切换禁用（D3D11 呈现
    /// 不可用会黑屏），启动已静默回退 sw。
    pub hw_gpu_available: bool,
    /// 设置视图的待写回项（帧末消费，见 main.rs）。
    pub pending_ui: Vec<PendingUiChange>,
    /// 图标目录浏览器是否打开（设置视图"开发"组入口；数据源 icon_catalog）。
    pub show_icon_browser: bool,

    /// 图标目录浏览器最近复制的图标（反馈行显示）。
    copied_icon: Option<(char, &'static str)>,
    /// 待执行的真实交互动作（开播/跳目录/源保存，帧末 main.rs 消费）。
    pub shell_actions: Vec<ShellAction>,
    /// 当前源浏览根（main.rs 在请求根目录时记录；面包屑相对段拆解用）。
    pub source_root: Option<PathBuf>,
    /// 侧栏切换 tab 时置 true，main.rs 帧末消费调 ensure_data 刷新。
    pub pending_refresh: bool,

    /// R12：媒体索引就绪缓存 (source_id, is_ready, timestamp)。
    /// 避免每帧查 DB；5 秒或源切换时刷新。
    index_ready_cache: Option<(i64, bool, Instant)>,
    /// 封面纹理缓存（key = cover_path；后台 VFS read_full + image 解码 →
    /// ColorImage 回流，UI 线程 ctx.load_texture 存 TextureHandle）。
    /// 统一走 VFS（本地源 LocalFs::read = std::fs::read；SMB/WebDAV 走认证
    /// VFS，勿用 file:// URI——Windows SMB redirector 与 pava_smb 认证不同路径）。
    pub(crate) cover_cache: std::collections::HashMap<String, CoverState>,
    /// 待加载封面 (cover_path, source_id)：渲染发现 cache miss 即入队（去重），
    /// main.rs 帧末 spawn 后台 VFS read_full + image 解码。
    pub pending_cover_load: Vec<(String, i64)>,
    /// 抽帧缩略图纹理缓存（key = thumb_gen::thumb_key；后台 mpv headless
    /// 抽帧 → ColorImage 回流，UI 线程 ctx.load_texture 存 TextureHandle）。
    /// 仅 cover_path 为 None 且 cover_probed=true 的条目走此管线。
    pub(crate) thumb_cache: std::collections::HashMap<String, CoverState>,
    /// 待抽帧 (source_id, folder_path)：渲染发现 thumb cache miss 即入队（去重），
    /// main.rs 帧末 spawn 后台 headless mpv 抽帧。
    pub pending_thumb_gen: Vec<(i64, String)>,
}

/// 封面纹理缓存态（UI 线程持有 TextureHandle）。
/// Loading 态省略：pending_cover_load 去重队列已覆盖"已派发未回流"，
/// cache miss（None）= 待入队，Some(Ready) = 显示，Some(Failed) = 不重试。
pub(crate) enum CoverState {
    Ready(egui::TextureHandle),
    Failed,
}


/// 设置视图的待写回全局设置项（帧末 main.rs 消费：写 config + 落盘 +
/// 字体重装 / 明暗回流播放会话；渲染路径额外触发会话重建）。
#[derive(Debug, Clone)]
pub enum PendingUiChange {
    Theme(dyplayer_store::ThemeMode),
    Accent(u8),
    Font(FontChoice),
    Startup(StartupView),
    /// 渲染路径（"sw" / "gpu"，会话重建型：render context 与 wid 呈现
    /// 互斥无法热切，main.rs drop 会话后按新配置重建并续位进度）。
    /// glow 后端不构造（渲染路径固定 GL 互操作）。
    #[cfg_attr(not(feature = "backend-wgpu"), allow(dead_code))]
    RenderPath(String),
    /// 画质档位（"high" / "balanced" / "low-power"，会话重建型：mpv
    /// 初始化期选项 scale/cscale/interpolation 等，运行时改需重建会话）。
    QualityPreset(String),
    /// 引擎高级参数（name=value 列表，会话重建型：mpv 初始化期选项）。
    ExtraArgs(Vec<String>),
}

/// 播放记录清除目标（右键单条 / 清除全部 / 分区清除全部）。
#[derive(Debug, Clone)]
pub enum ClearHistoryTarget {
    /// 删单条记录（右键单行）。
    Single {
        source_id: i64,
        path: String,
        title: String,
    },
    /// 清空全部播放记录（最近播放视图"清除全部"）。
    All,
    /// 清空所有非媒体库单文件记录（首页"继续观看"清除全部）。
    AllSingles {
        lib_folder_paths: Vec<String>,
    },
    /// 清空所有媒体库相关记录（首页"最近访问的媒体库"清除全部）。
    AllLibs {
        lib_folder_paths: Vec<String>,
    },
    /// 清空某媒体库文件夹下所有记录（右键媒体库卡片）。
    Folder {
        folder_path: String,
        display_name: String,
    },
}

/// Shell 真实交互动作（帧末 main.rs 消费执行）。
#[derive(Debug, Clone)]
pub enum ShellAction {
    /// 最近播放恢复（含拖拽 source_id=0）或收藏文件开播。
    ResumeRecent { source_id: i64, path: PathBuf, http_headers: Option<String> },
    /// 文件夹开播（媒体库卡片主体点击）。
    PlayFolder { source_id: i64, dir: PathBuf },
    /// 单文件开播（所在目录组队并定位该文件）。
    PlayFile { source_id: i64, dir: PathBuf, file: PathBuf },
    /// 收藏文件夹：切到该源并浏览该目录。
    OpenFolder { source_id: i64, dir: PathBuf },
    /// 当前源内进入子目录。
    BrowseInto { dir: PathBuf },
    /// 回到当前源根目录（面包屑源名段点击）。
    BrowseRoot,
    /// 添加本地源（rfd 文件夹选择器）。
    AddSourceLocal,
    /// 添加远程源（WebDAV / SMB 表单对话框）。
    AddSourceRemote(SourceKind),
    /// 收藏/取消收藏：(source_id, path, title, is_dir)。
    ToggleFavorite {
        source_id: i64,
        path: PathBuf,
        title: String,
        is_dir: bool,
    },
    /// 编辑源表单保存（落库）。
    SaveSource(Box<EditSourceForm>),
    /// 删除媒体源（五表级联 + keyring 凭证清理，main.rs 帧末执行）。
    DeleteSource { source_id: i64, name: String },
    /// 加入媒体库（后台异步：list_dir 子文件夹探测 cover.jpg 约定封面 +
    /// add_media_library 一次完成，UI 不阻塞；request_repaint 回流刷新）。
    AddToLib {
        source_id: i64,
        folder_path: PathBuf,
        name: String,
    },
    /// 清除播放记录（帧末 main.rs 执行 store 删除 + browser.refresh_recent）。
    ClearHistory(ClearHistoryTarget),
}

/// 编辑/添加源表单（同一定制弹框两种模式）：`source_id = Some` 编辑既有
/// 源（预填真实配置，密码留空 = 保持原凭证）；`None` 新建源。
#[derive(Clone, Debug)]
pub struct EditSourceForm {
    /// 目标源 id；None = 新建。
    pub source_id: Option<i64>,
    pub kind: SourceKind,
    pub name: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub accept_invalid_certs: bool,
}

impl EditSourceForm {
    fn from_source(s: &MediaSource) -> Self {
        Self {
            source_id: Some(s.id),
            kind: s.kind,
            name: s.name.clone(),
            endpoint: s.endpoint.clone(),
            username: s.username.clone(),
            password: String::new(),
            accept_invalid_certs: s.accept_invalid_certs,
        }
    }

    /// 新建模式（源卡"添加"菜单入口；密码必填语义与编辑不同）。
    fn new_source(kind: SourceKind) -> Self {
        Self {
            source_id: None,
            kind,
            name: String::new(),
            endpoint: String::new(),
            username: String::new(),
            password: String::new(),
            accept_invalid_certs: false,
        }
    }
}

/// 弹框内"测试连接"的后台任务状态（Receiver 非 Clone/不可入 form，
/// 持有于 BrowseUiState；弹框关闭时丢弃）。
struct SourceTest {
    rx: std::sync::mpsc::Receiver<Result<usize, String>>,
    result: Option<Result<usize, String>>,
}

impl SourceTest {
    fn running(&self) -> bool {
        self.result.is_none()
    }
}

impl Default for BrowseUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowseUiState {
    pub fn new() -> Self {
        Self {
            exit_requested: false,
            view: BrowseTab::Home,
            theme_idx: 3,
            search: String::new(),
            selected_source: None,
            pending_source: None,
            edit: None,
            confirm_delete: None,
            confirm_clear_history: None,
            source_test: None,
            notice: None,
            media_lib: Vec::new(),
            lib_stats: std::collections::HashMap::new(),
            pending_lib_stats: Vec::new(),
            lib_stats_stamp: None,
            index_hits: Vec::new(),
            index_hits_query: String::new(),
            index_hits_source: 0,
            ui_font: FontChoice::App,
            ui_startup: StartupView::default(),
            ui_render_gpu: false,
            ui_quality_preset: "high".into(),
            ui_extra_args: String::new(),
            hw_gpu_available: true,
            pending_ui: Vec::new(),
            show_icon_browser: false,

            copied_icon: None,
            shell_actions: Vec::new(),
            source_root: None,
            pending_refresh: false,

            index_ready_cache: None,
            cover_cache: std::collections::HashMap::new(),
            pending_cover_load: Vec::new(),
            thumb_cache: std::collections::HashMap::new(),
            pending_thumb_gen: Vec::new(),
        }
    }

    /// 进入 Browse 页时同步全局 UI 设置镜像（config → state）。
    pub fn sync_ui_cfg(&mut self, ui: &dyplayer_store::UiConfig, engine: &dyplayer_store::EngineConfig) {
        let max = crate::theme::palettes(false).len() as u8;
        self.theme_idx = (ui.accent.min(max.saturating_sub(1))) as usize;
        self.ui_font = ui.font.clone();
        self.ui_startup = ui.startup_view;
        self.ui_render_gpu = engine.render_path.eq_ignore_ascii_case("gpu");
        self.ui_quality_preset = engine.quality_preset.clone();
        self.ui_extra_args = engine.extra_args.join("\n");
    }

    /// 进入时刷新真实数据（recent / favorites / 媒体库缓存，
    /// 每次进入主动加载最新）。
    pub fn ensure_data(&mut self, store: &Arc<Store>, browser: &mut BrowserState) {
        browser.refresh_recent(store);
        browser.refresh_favorites(store);
        self.selected_source = browser.active_source;
        self.refresh_media_lib(store);
        // 播放历史变化（播放/续播/看完落库）→ 统计过期，清空重拉。
        // recent 首条按 updated_at DESC，任何变化都会体现在首条。
        let stamp = browser.recent.first().map(|r| r.updated_at.clone());
        if stamp != self.lib_stats_stamp {
            self.lib_stats_stamp = stamp;
            self.lib_stats.clear();
        }
    }


    fn refresh_media_lib(&mut self, store: &Arc<Store>) {
        match store.list_media_library() {
            Ok(list) => {
                // 移除的条目同步清理统计缓存与待统计队列。
                self.lib_stats
                    .retain(|id, _| list.iter().any(|e| e.id == *id));
                self.pending_lib_stats
                    .retain(|(id, _, _)| list.iter().any(|e| e.id == *id));
                // 移除条目的封面纹理清理（TextureHandle drop 释放显存）。
                self.cover_cache.retain(|k, _| {
                    list.iter().any(|e| e.cover_path.as_deref() == Some(k.as_str()))
                });
                // 移除条目的抽帧缩略图纹理清理。
                self.thumb_cache.retain(|k, _| {
                    list.iter().any(|e| crate::thumb_gen::thumb_key(e.source_id, &e.folder_path) == *k)
                });
                // 存量 cover_probed=false 条目补探测封面（后台 AddToLib 幂等
                // 更新 cover_path + 置 cover_probed=true；探测后不再入队）。
                // 去重：shell_actions 里已有同源同路径 AddToLib 时跳过，避免
                // 探测未完成期间每 2s 重复 spawn。
                for e in &list {
                    if !e.cover_probed
                        && !self.shell_actions.iter().any(|a| {
                            matches!(
                                a,
                                ShellAction::AddToLib { source_id, folder_path, .. }
                                    if *source_id == e.source_id
                                        && folder_path.to_string_lossy() == e.folder_path
                            )
                        })
                    {
                        self.shell_actions.push(ShellAction::AddToLib {
                            source_id: e.source_id,
                            folder_path: PathBuf::from(&e.folder_path),
                            name: e.display_name.clone(),
                        });
                    }
                }
                self.media_lib = list;
            }
            Err(e) => tracing::warn!("读取媒体库失败: {e}"),
        }
    }

    /// 渲染路径请求统计：无缓存且不在待统计队列 → 入队（main.rs 帧末
    /// 批量 spawn；结果经 lib_stats 回流后本方法自然短路）。
    pub fn request_lib_stats(&mut self, entry: &MediaLibEntry) {
        if self.lib_stats.contains_key(&entry.id)
            || self
                .pending_lib_stats
                .iter()
                .any(|(id, _, _)| *id == entry.id)
        {
            return;
        }
        self.pending_lib_stats
            .push((entry.id, entry.source_id, entry.folder_path.clone()));
    }

    /// 退出时清理瞬态（搜索词 / 提示 / 编辑面板 / 删除确认 / 测试任务 /
    /// 图标弹窗 / 索引搜索缓存），保留视图与主题选择。
    pub fn reset_transient(&mut self) {
        self.search.clear();
        self.notice = None;
        self.edit = None;
        self.confirm_delete = None;
        self.source_test = None;
        self.show_icon_browser = false;
        self.clear_index_search();
    }

    /// 打开"添加远程源"定制弹框（源卡"添加"菜单；与编辑源同一弹框工艺）。
    pub fn open_add_source(&mut self, kind: SourceKind) {
        self.source_test = None;
        self.edit = Some(EditSourceForm::new_source(kind));
    }

    /// 清空媒体源视图的索引搜索缓存（切源/删源时调用：命中属于旧源）。
    pub fn clear_index_search(&mut self) {
        self.index_hits.clear();
        self.index_hits_query.clear();
    }

    pub fn notify(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

pub fn ui(
    parent: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    browser: &BrowserState,
    store: &Arc<Store>,
    theme_mode: &mut dyplayer_store::ThemeMode,
    dark: bool,
) {
    let palette = palettes(dark)[state.theme_idx];

    // ESC：图标弹窗 → 编辑面板 → 删除确认 → 回播放页，逐层关闭
    // （exit_requested 由 main.rs Browse 分支消费为 Screen::Player）。
    if parent.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
        if state.show_icon_browser {
            state.show_icon_browser = false;
        } else if state.edit.is_some() {
            state.edit = None;
        } else if state.confirm_clear_history.is_some() {
            state.confirm_clear_history = None;
        } else if state.confirm_delete.is_some() {
            state.confirm_delete = None;
        } else {
            state.exit_requested = true;
        }
    }

    // gutter 底色由调用方（main.rs）在标题栏之前铺满全屏——
    // 这里若用 max_rect 再画一次会盖住先渲染的标题栏（max_rect 不随 panel 收缩）。

    egui::Panel::left("lib-side")
        .exact_size(218.0)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(palette.panel)
                .stroke(Stroke::NONE)
                .corner_radius(14.0)
                .inner_margin(egui::Margin::same(10))
                .outer_margin(egui::Margin {
                    left: 8,
                    right: 8,
                    top: 8,
                    bottom: 8,
                }),
        )
        .show(parent, |ui| {
            ui_side(ui, state, sources, palette)
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(palette.panel)
                .stroke(Stroke::NONE)
                .corner_radius(14.0)
                .inner_margin(egui::Margin::same(16))
                .outer_margin(egui::Margin {
                    left: 0,
                    right: 8,
                    top: 8,
                    bottom: 8,
                }),
        )
        .show(parent, |ui| {
            ui_content(ui, state, sources, browser, store, palette, theme_mode, dark)
        });

    // 编辑源 modal（最上层：遮罩 + 居中卡片）。
    ui_edit_modal(parent, state, palette);
    // 删除源确认弹窗（与编辑弹窗互斥，右键源行触发）。
    ui_confirm_delete(parent, state, palette);
    // 播放记录清除确认弹窗（右键单行 / "清除全部"按钮触发）。
    ui_confirm_clear_history(parent, state, palette);
    // 图标目录浏览器（开发工具，egui Window 浮层）。
    icon_browser_window(parent, state, palette);

}

// ---------------------------------------------------------------------------
// 侧栏
// ---------------------------------------------------------------------------

fn ui_side(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    palette: Palette,

) {
    // 标题区：固定 34px 高，文本垂直居中（此前绝对坐标画在区域底部，
    // 与首个导航项只剩 2px 间距）。
    let (title_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    ui.painter().text(
        pos2(title_rect.left() + 12.0, title_rect.center().y),
        Align2::LEFT_CENTER,
        "DyPlayer",
        lib_font(16.0),
        palette.text,
    );
    ui.add_space(4.0);

    for (view, glyph, label) in [
        (BrowseTab::Home, Glyph::Home, "首页"),
        (BrowseTab::MediaLib, Glyph::Grid, "媒体库"),
        (BrowseTab::Library, Glyph::Folder, "媒体源"),
        (BrowseTab::Recent, Glyph::History, "最近播放"),
        (BrowseTab::Favorites, Glyph::Star, "收藏"),
    ] {
        if nav_row(ui, view, glyph, label, state.view == view, palette).clicked() {
            state.view = view;
            state.pending_refresh = true;
        }
    }

    ui.add_space(8.0);
    paint_hline(ui, palette);
    ui.add_space(14.0);

    // 分组标题行：媒体源 + 右缘"+"（from_min_size 收边防溢出）。
    // 点击弹添加菜单（本地文件夹 / WebDAV / SMB）——首页不再展示媒体源
    // 卡片，新增源入口收敛到侧栏。菜单向下弹（标题行在侧栏上部，空间充足；
    // 贴底元素才需要 TOP_START 向上弹）。
    {
        let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
        let p = ui.painter();
        p.text(
            pos2(head.left() + 4.0, head.center().y),
            Align2::LEFT_CENTER,
            "媒体源",
            lib_font(12.0),
            palette.dim,
        );
        let btn = Rect::from_min_size(
            pos2(head.right() - 20.0, head.center().y - 10.0),
            vec2(20.0, 20.0),
        );
        let plus = ui.interact(btn, Id::unique("lib-side-add"), Sense::click());
        let t = ui.ctx().animate_value_with_time(
            Id::unique("lib-side-add-h"),
            plus.hovered() as u8 as f32,
            0.09,
        );
        p.rect_filled(btn, CornerRadius::same(10), mix(palette.panel, palette.surface_hover, t));
        paint_glyph(
            p,
            Glyph::Plus,
            Rect::from_center_size(btn.center(), vec2(12.0, 12.0)),
            if plus.hovered() {
                palette.accent
            } else {
                palette.secondary
            },
        );
        egui::Popup::menu(&plus).show(|ui| {
            ui.set_min_width(190.0);
            if ui.button("本地文件夹…").clicked() {
                state.shell_actions.push(ShellAction::AddSourceLocal);
            }
            if ui.button("WebDAV…").clicked() {
                state
                    .shell_actions
                    .push(ShellAction::AddSourceRemote(SourceKind::WebDav));
            }
            if ui.button("SMB…").clicked() {
                state
                    .shell_actions
                    .push(ShellAction::AddSourceRemote(SourceKind::Smb));
            }
        });
        plus.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    ui.add_space(6.0);

    for (i, source) in sources.iter().enumerate() {
        let (resp, edit) = source_row(ui, source, state.selected_source == Some(i), palette);
        // 右键 = 删除入口（确认弹窗兜底，非直接删除）。
        if resp.secondary_clicked() {
            state.confirm_delete = Some((source.id, source.name.clone()));
        } else if edit {
            // 行内编辑钮（hover 右缘浮出）：打开与首页同源的编辑弹框。
            state.edit = Some(EditSourceForm::from_source(source));
        } else if resp.clicked() {
            state.selected_source = Some(i);
            state.pending_source = Some(i);
            state.view = BrowseTab::Library;
        }
        resp.on_hover_text("左键打开 · 右键删除");
    }
    if sources.is_empty() {
        ui.painter().text(
            pos2(
                ui.next_widget_position().x + 4.0,
                ui.next_widget_position().y,
            ),
            Align2::LEFT_CENTER,
            "暂无媒体源",
            lib_font(11.5),
            palette.dim,
        );
        ui.add_space(14.0);
    }

    // 底部：设置导航行 → 返回播放页按钮（bottom_up：先画在最底；明暗三态与
    // 主题色块已迁入设置视图）。本模块即 Browse 主界面 shell，
    // 此按钮为主界面回播放页入口（ESC 同义）。
    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        if pill_button(ui, "返回播放页 (ESC)", palette, palette.danger).clicked() {
            state.exit_requested = true;
        }
        ui.add_space(8.0);
        if nav_row(
            ui,
            BrowseTab::Settings,
            Glyph::Settings,
            "设置",
            state.view == BrowseTab::Settings,
            palette,
        )
        .clicked()
        {
            state.view = BrowseTab::Settings;
        }
        ui.add_space(2.0);
    });
}

/// 导航行：胶囊选中态 + accent 竖条 + 图标 + 文本（Musify nav_row 工艺）。
fn nav_row(
    ui: &mut Ui,
    id_seed: BrowseTab,
    glyph: Glyph,
    label: &str,
    selected: bool,
    palette: Palette,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let hovered = resp.hovered();
    let t = ui.ctx().animate_value_with_time(
        Id::unique("lib-nav").with(id_seed as u8),
        hovered as u8 as f32,
        0.09,
    );
    let p = ui.painter();

    let bg = if selected {
        palette.surface_active
    } else if t > 0.01 {
        mix(palette.panel, palette.surface_hover, t)
    } else {
        palette.panel
    };
    if bg != palette.panel {
        p.rect_filled(rect, CornerRadius::same(8), bg);
    }
    if selected {
        p.rect_filled(
            Rect::from_min_size(
                pos2(rect.left() + 4.0, rect.top() + 8.0),
                vec2(3.0, rect.height() - 16.0),
            ),
            CornerRadius::same(2),
            palette.accent,
        );
    }

    let icon_color = if selected {
        palette.accent
    } else {
        mix(palette.secondary, palette.text, t)
    };
    let icon_rect =
        Rect::from_center_size(pos2(rect.left() + 26.0, rect.center().y), vec2(17.0, 17.0));
    paint_glyph(p, glyph, icon_rect, icon_color);
    p.text(
        pos2(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        lib_font(13.5),
        if selected {
            palette.text
        } else {
            mix(palette.secondary, palette.text, t * 0.7)
        },
    );
    resp
}

/// 源行：kind 图标 + 名称 + 类型 secondary 标签。
/// 侧栏媒体源行：类型 glyph + 名称/类型双行 + hover 行底；hover 时右缘
/// 浮出编辑钮（Pencil，后注册赢 click 不触发行选中）。返回 (行响应, 编辑点击)。
fn source_row(
    ui: &mut Ui,
    source: &MediaSource,
    selected: bool,
    palette: Palette,
) -> (Response, bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    // 编辑钮 interact 前置且无条件注册（保持 resp→er 注册序：er 后注册赢
    // click，不触发行选中）。条件注册（t>0.01 才注册）会与行动画互相振荡：
    // 指针移入编辑钮 → 行 hover 被顶层取代 → t 衰减 → 热区消失 → 行
    // hover 恢复 → t 上升 → 热区重现……循环抖动。热区常驻后 hover 驱动
    // 合并两者即可稳定。
    let edit_btn = Rect::from_min_size(
        pos2(rect.right() - 26.0, rect.center().y - 11.0),
        vec2(22.0, 22.0),
    );
    let er = ui.interact(
        edit_btn,
        Id::unique("lib-src-edit").with(source.id),
        Sense::click(),
    );
    let t = ui.ctx().animate_value_with_time(
        Id::unique("lib-src").with(source.id),
        (resp.hovered() || er.hovered()) as u8 as f32,
        0.09,
    );
    let p = ui.painter();

    let bg = if selected {
        palette.surface_active
    } else {
        mix(palette.panel, palette.surface_hover, t)
    };
    p.rect_filled(rect, CornerRadius::same(8), bg);

    paint_glyph(
        p,
        source_kind_glyph(source.kind),
        Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(16.0, 16.0)),
        if selected {
            palette.accent
        } else {
            palette.secondary
        },
    );
    // 名称截断：右缘预留编辑钮热区（hover 浮出时遮挡长名，常态留白避让）。
    paint_left_center_truncated(
        p,
        pos2(rect.left() + 40.0, rect.center().y - 5.0),
        &source.name,
        lib_font(13.0),
        palette.text,
        (rect.width() - 40.0 - 30.0).max(40.0),
    );
    p.text(
        pos2(rect.left() + 40.0, rect.center().y + 8.0),
        Align2::LEFT_CENTER,
        source.kind.label(),
        lib_font(10.5),
        palette.dim,
    );

    // 编辑钮绘制（hover 浮出；热区已前置常驻注册）。
    // 点击仅在视觉可见（t>0.5）时接受，防隐藏热区误触。
    let mut edit_clicked = false;
    if t > 0.01 {
        let et = ui.ctx().animate_value_with_time(
            Id::unique("lib-src-edit-h").with(source.id),
            er.hovered() as u8 as f32,
            0.09,
        );
        let p = ui.painter();
        p.circle_filled(
            edit_btn.center(),
            11.0,
            mix(palette.panel, palette.surface_active, et.max(0.4)),
        );
        paint_glyph(
            p,
            Glyph::Pencil,
            Rect::from_center_size(edit_btn.center(), vec2(11.0, 11.0)),
            if er.hovered() {
                palette.accent
            } else {
                palette.secondary
            },
        );
        if er.clicked() && t > 0.5 {
            edit_clicked = true;
        }
        er.on_hover_cursor(egui::CursorIcon::PointingHand);
    }

    (resp, edit_clicked)
}

// ---------------------------------------------------------------------------
// 主区
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn ui_content(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    browser: &BrowserState,
    store: &Arc<Store>,
    palette: Palette,
    theme_mode: &mut dyplayer_store::ThemeMode,
    dark: bool,
) {
    let view = state.view;
    if view == BrowseTab::Settings {
        // 设置页无搜索 / 计数：简化顶栏（仅标题），主体为分组设置卡。
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        ui.painter().text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            "设置",
            lib_font(18.0),
            palette.text,
        );
        ui.add_space(10.0);
        ui_settings(ui, state, palette, theme_mode, dark);
        return;
    }
    let (title, count) = match view {
        BrowseTab::Home => ("首页", None),
        BrowseTab::MediaLib => ("媒体库", Some(state.media_lib.len())),
        BrowseTab::Library => ("媒体源", None),
        BrowseTab::Recent => ("最近播放", Some(browser.recent.len())),
        BrowseTab::Favorites => ("收藏", Some(browser.favorites.len())),
        BrowseTab::Settings => unreachable!(),
    };
    ui_header(ui, state, title, count, palette);
    ui.add_space(10.0);

    match view {
        BrowseTab::Home => ui_home(ui, state, sources, browser, store, palette),
        BrowseTab::MediaLib => ui_medialib(ui, state, sources, store, palette),
        BrowseTab::Library => ui_library(ui, state, sources, browser, store, palette),
        BrowseTab::Recent => ui_recent(ui, state, sources, browser, palette),
        BrowseTab::Favorites => ui_favorites(ui, state, sources, browser, palette),
        BrowseTab::Settings => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// 设置视图（全局设置：外观 / 字体 / 启动；写回 config，帧末由 main.rs 消费）
// ---------------------------------------------------------------------------

/// 设置分组卡：surface 底圆角卡 + 组标题 + 内容（外观 / 字体 / 启动同构）。
fn settings_group(ui: &mut Ui, palette: Palette, title: &str, body: impl FnOnce(&mut Ui)) {
    let avail = ui.available_width();
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::NONE)
        .corner_radius(12.0)
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_width((avail - 36.0).max(200.0));
            let (trect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
            ui.painter().text(
                pos2(trect.left(), trect.center().y),
                Align2::LEFT_CENTER,
                title,
                lib_font(12.5),
                palette.secondary,
            );
            ui.add_space(8.0);
            body(ui);
        });
    ui.add_space(12.0);
}

/// 设置行：左标签（固定列宽）+ 右控件区。
fn settings_row(ui: &mut Ui, palette: Palette, label: &str, cells: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(96.0, 28.0), Sense::hover());
        ui.painter().text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            label,
            lib_font(12.5),
            palette.secondary,
        );
        cells(ui);
    });
    ui.add_space(8.0);
}

/// 选择胶囊（自适应宽）：选中 accent 实底，hover 微亮（明暗 / 字体 / 启动共用）。
/// 文本色在 layout 时确定——`Painter::galley` 的第三参数只是 fallback（job
/// 已带色时不生效），选中态 on_accent 必须在 layout 传入才真正渲染。
fn seg_pill(ui: &mut Ui, label: &str, selected: bool, palette: Palette) -> Response {
    let text_color = if selected {
        palette.on_accent
    } else {
        palette.secondary
    };
    let gal = ui
        .painter()
        .layout_no_wrap(label.to_string(), lib_font(11.5), text_color);
    let w = gal.size().x + 22.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
    let pr = ui.painter();
    if selected {
        pr.rect_filled(rect, CornerRadius::same(12), palette.accent);
    } else if resp.hovered() {
        pr.rect_filled(rect, CornerRadius::same(12), palette.surface_hover);
    }
    pr.galley(
        rect.min + vec2(11.0, (rect.height() - gal.size().y) / 2.0),
        gal,
        text_color,
    );
    resp
}

/// 设置主体：外观（明暗 + 主题色）/ 字体（应用自带 / 系统中文字体）/ 启动视图。
/// 每次点击即时更新镜像并 push pending_ui，由 main.rs 帧末写回 config
/// 并落盘（字体切换重装字体链，明暗回流播放会话镜像）。
fn ui_settings(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    palette: Palette,
    theme_mode: &mut dyplayer_store::ThemeMode,
    dark: bool,
) {
    egui::ScrollArea::vertical()
        .id_salt("lib-settings")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            settings_group(ui, palette, "外观", |ui| {
                settings_row(ui, palette, "明暗模式", |ui| {
                    for (mode, label) in [
                        (dyplayer_store::ThemeMode::System, "跟随系统"),
                        (dyplayer_store::ThemeMode::Light, "浅色"),
                        (dyplayer_store::ThemeMode::Dark, "深色"),
                    ] {
                        let resp = seg_pill(ui, label, *theme_mode == mode, palette);
                        if resp.clicked() {
                            *theme_mode = mode;
                            state.pending_ui.push(PendingUiChange::Theme(mode));
                        }
                    }
                });
                settings_row(ui, palette, "主题色", |ui| {
                    for (i, p) in palettes(dark).iter().enumerate() {
                        let selected = state.theme_idx == i;
                        let (rect, resp) =
                            ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                        let pr = ui.painter();
                        pr.rect_filled(rect.shrink(2.0), CornerRadius::same(6), p.accent);
                        if selected {
                            pr.rect_stroke(
                                rect.shrink(0.5),
                                CornerRadius::same(7),
                                Stroke::new(1.8, palette.text),
                                StrokeKind::Inside,
                            );
                        }
                        if resp.clicked() {
                            state.theme_idx = i;
                            state.pending_ui.push(PendingUiChange::Accent(i as u8));
                        }
                        resp.on_hover_text(p.name);
                    }
                });

                settings_row(ui, palette, "界面字体", |ui| {
                    if seg_pill(ui, "应用自带", state.ui_font == FontChoice::App, palette)
                        .on_hover_text("HarmonyOS Sans SC（应用内置，全平台观感一致）")
                        .clicked()
                    {
                        state.ui_font = FontChoice::App;
                        state.pending_ui.push(PendingUiChange::Font(state.ui_font.clone()));
                    }
                    let sys_selected = matches!(state.ui_font, FontChoice::System(_));
                    if seg_pill(ui, "系统字体", sys_selected, palette)
                        .on_hover_text("Windows 内置中文字体（切换即时生效）")
                        .clicked()
                        && !sys_selected
                    {
                        let name = crate::player_app::available_system_cjk_fonts()
                            .first()
                            .copied()
                            .unwrap_or("微软雅黑");
                        state.ui_font = FontChoice::System(name.into());
                        state.pending_ui.push(PendingUiChange::Font(state.ui_font.clone()));
                    }
                });
                // 当前为系统字体时展开候选列表（文件存在过滤后的内置中文字体）。
                let sys_name = match &state.ui_font {
                    FontChoice::System(n) => Some(n.clone()),
                    FontChoice::App => None,
                };
                if let Some(current) = sys_name {
                    settings_row(ui, palette, "中文字体", |ui| {
                        for name in crate::player_app::available_system_cjk_fonts() {
                            if seg_pill(ui, name, current == name, palette).clicked() {
                                state.ui_font = FontChoice::System(name.into());
                                state
                                    .pending_ui
                                    .push(PendingUiChange::Font(state.ui_font.clone()));
                            }
                        }
                    });
                }
            });

            // 渲染路径（会话重建型，区别于播放页设置窗的热切项）：render
            // context 与 wid 自管呈现互斥，改动由 main.rs 重建 PlayerSession
            //（进度续位，留在媒体库页不跳屏）。
            settings_group(ui, palette, "播放", |ui| {
                settings_row(ui, palette, "渲染路径", |ui| {
                    #[cfg(feature = "backend-wgpu")]
                    {
                        if seg_pill(ui, "默认 (SW)", !state.ui_render_gpu, palette)
                            .on_hover_text(
                                "mpv 软渲染 + wgpu 纹理上传：全场景稳定，高分辨率源吃 CPU",
                            )
                            .clicked()
                            && state.ui_render_gpu
                        {
                            state.ui_render_gpu = false;
                            state
                                .pending_ui
                                .push(PendingUiChange::RenderPath("sw".into()));
                        }
                        let gpu_hover = if state.hw_gpu_available {
                            "mpv 自管 D3D11 渲染（wid 宿主子窗口，呈现由 mpv 承担）：高分辨率大幅省 CPU"
                        } else {
                            "当前环境仅有软件 WARP 适配器（无任何 GPU 驱动）：已禁用并回退软件渲染"
                        };
                        if seg_pill(ui, "GPU", state.ui_render_gpu, palette)
                            .on_hover_text(gpu_hover)
                            .clicked()
                            && !state.ui_render_gpu
                            && state.hw_gpu_available
                        {
                            state.ui_render_gpu = true;
                            state
                                .pending_ui
                                .push(PendingUiChange::RenderPath("gpu".into()));
                        }
                    }
                    #[cfg(not(feature = "backend-wgpu"))]
                    {
                        seg_pill(ui, "GL 互操作", true, palette).on_hover_text(
                            "mpv OpenGL 互操作渲染（glow 后端）：mpv 直接渲染到 GL FBO，\
                             零拷贝无 CPU 上传，老设备兼容路径",
                        );
                    }
                });
                // 画质档位（会话重建型）：高/平衡/低功耗三选一。
                // 低功耗档降级采样器避免弱 iGPU（N2940 Gen7）GPU 反压解码。
                settings_row(ui, palette, "画质档位", |ui| {
                    let current = crate::player_app::QualityPreset::from_config(
                        &state.ui_quality_preset,
                    );
                    for preset in crate::player_app::QualityPreset::ALL {
                        if seg_pill(ui, preset.label(), current == preset, palette)
                            .on_hover_text(match preset {
                                crate::player_app::QualityPreset::High => {
                                    "高（默认）：mpv 默认 gpu-hq profile（lanczos + sigmoid + \
                                     interpolation），零回归。强 GPU 推荐"
                                }
                                crate::player_app::QualityPreset::Balanced => {
                                    "平衡：spline36 采样器 + interpolation=auto，\
                                     中端 GPU 降一档保 sigmoid"
                                }
                                crate::player_app::QualityPreset::LowPower => {
                                    "低功耗：bilinear + 关 sigmoid/interpolation，\
                                     弱 iGPU（N2940/Atom 类）避免 GPU 反压解码管线"
                                }
                            })
                            .clicked()
                        {
                            state.ui_quality_preset = preset.config_value().into();
                            state
                                .pending_ui
                                .push(PendingUiChange::QualityPreset(preset.config_value().into()));
                        }
                    }
                });
                // 引擎高级参数（会话重建型）：多行文本编辑器，每行一个 name=value。
                // 失焦时 push pending_ui；用户可覆盖画质档位与配速的注入参数。
                // 布局：左编辑器（无焦点也加描边提高可见度）+ 右预设按钮垂直排列，
                // hover tooltip 锚定按钮底部（非默认的上方居中）。
                settings_row(ui, palette, "高级参数", |ui| {
                    ui.horizontal_top(|ui| {
                        // 左：编辑器（scope 内改 inactive 描边，clone-on-write 本地副本）。
                        ui.scope(|ui| {
                            ui.visuals_mut().widgets.inactive.bg_stroke =
                                egui::Stroke::new(1.0, palette.dim);
                            let resp = ui.add(
                                egui::TextEdit::multiline(&mut state.ui_extra_args)
                                    .desired_width(180.0)
                                    .desired_rows(5)
                                    .font(egui::TextStyle::Monospace)
                                    .interactive(true),
                            );
                            if resp.lost_focus() {
                                let args: Vec<String> = state
                                    .ui_extra_args
                                    .lines()
                                    .map(|l| l.trim().to_string())
                                    .filter(|l| !l.is_empty())
                                    .collect();
                                state.pending_ui.push(PendingUiChange::ExtraArgs(args));
                            }
                        });
                        // 右：预设按钮垂直排列，tooltip 锚定按钮底部。
                        ui.vertical(|ui| {
                            for preset in crate::player_app::ExtraArgsPreset::ALL {
                                let btn = ui.add(
                                    egui::Button::new(preset.label()).min_size(vec2(52.0, 0.0)),
                                );
                                let tooltip: &str = match preset {
                                    crate::player_app::ExtraArgsPreset::Default => {
                                        "清空高级参数（全靠画质档位 + 代码注入）"
                                    }
                                    crate::player_app::ExtraArgsPreset::Cinema => {
                                        "影院：ewa_lanczossharp + sigmoid + interpolation + \
                                         dither，强 GPU 高质量"
                                    }
                                    crate::player_app::ExtraArgsPreset::Balanced => {
                                        "平衡：spline36 + interpolation=auto，中端 GPU"
                                    }
                                    crate::player_app::ExtraArgsPreset::LowPower => {
                                        "低功耗+：bilinear + 关 sigmoid/interp + threads=4 + \
                                         cache-secs=20，弱 iGPU（N2940/Atom）+ 软解防饥饿"
                                    }
                                };
                                if btn.hovered() {
                                    let pos = btn.rect.left_bottom() + egui::vec2(0.0, 6.0);
                                    egui::Area::new(btn.id.with("htip"))
                                        .order(egui::Order::Tooltip)
                                        .fixed_pos(pos)
                                        .interactable(false)
                                        .show(ui.ctx(), |ui| {
                                            egui::Frame::new()
                                                .fill(palette.surface)
                                                .stroke(egui::Stroke::new(1.0, palette.dim))
                                                .corner_radius(4.0)
                                                .inner_margin(egui::Margin::same(6))
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        egui::RichText::new(tooltip).size(11.0),
                                                    );
                                                });
                                        });
                                }
                                if btn.clicked() {
                                    state.ui_extra_args = preset.args_text().into();
                                    let args: Vec<String> = state
                                        .ui_extra_args
                                        .lines()
                                        .map(|l| l.trim().to_string())
                                        .filter(|l| !l.is_empty())
                                        .collect();
                                    state.pending_ui.push(PendingUiChange::ExtraArgs(args));
                                }
                            }
                        });
                    });
                    ui.label(
                        egui::RichText::new("每行 name=value，如 video-sync=audio")
                            .color(palette.dim)
                            .size(11.0),
                    );
                });
                #[cfg(feature = "backend-wgpu")]
                if !state.hw_gpu_available {
                    settings_row(ui, palette, "", |ui| {
                        ui.label(
                            egui::RichText::new("仅有软件 WARP 适配器，GPU 渲染已禁用（软件渲染中）")
                                .color(palette.dim)
                                .weak(),
                        );
                    });
                }
            });
            settings_group(ui, palette, "启动", |ui| {
                settings_row(ui, palette, "启动时进入", |ui| {
                    for (v, label) in [
                        (StartupView::Library, "媒体库"),
                        (StartupView::Player, "播放页"),
                    ] {
                        if seg_pill(ui, label, state.ui_startup == v, palette).clicked() {
                            state.ui_startup = v;
                            state.pending_ui.push(PendingUiChange::Startup(v));
                        }
                    }
                });
            });
            if cfg!(debug_assertions) {
                settings_group(ui, palette, "开发", |ui| {
                    settings_row(ui, palette, "图标目录", |ui| {
                        if pill_button(ui, "浏览未使用图标", palette, palette.text).clicked() {
                            state.show_icon_browser = true;
                        }
                    });

                });
            }
            settings_group(ui, palette, "关于", |ui| {
                settings_row(ui, palette, "版本", |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(120.0, 18.0), Sense::hover());
                    ui.painter().text(
                        pos2(rect.left(), rect.center().y),
                        Align2::LEFT_CENTER,
                        env!("CARGO_PKG_VERSION"),
                        lib_font(12.5),
                        palette.text,
                    );
                });
                settings_row(ui, palette, "交流群", |ui| {
                    let mut buf = "811806197".to_string();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut buf)
                            .font(lib_font(12.5))
                            .text_color(palette.text)
                            .desired_width(90.0)
                            .clip_text(false)
                            .frame(egui::Frame::NONE),
                    );
                    if !resp.double_clicked() && resp.clicked() {
                        ui.ctx().copy_text(buf.clone());
                    }
                    resp.on_hover_text("单击复制 · 双击选中 · Ctrl+C");
                });
                settings_row(ui, palette, "个人主页", |ui| {
                    let url = "https://github.com/zhaohuaxs";
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(220.0, 18.0), Sense::click());
                    ui.painter().text(
                        pos2(rect.left(), rect.center().y),
                        Align2::LEFT_CENTER,
                        url,
                        lib_font(12.5),
                        palette.accent,
                    );
                    if resp.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("单击浏览器打开");
                });
            });
        });
}

/// 顶栏：视图片标题 + 计数 chip + 页内提示 + 搜索框（真实过滤）。
fn ui_header(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    title: &str,
    count: Option<usize>,
    palette: Palette,
) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let p = ui.painter();

    p.text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        title,
        lib_font(18.0),
        palette.text,
    );
    if let Some(n) = count {
        let chip = Rect::from_center_size(
            pos2(
                rect.left() + 100.0 + n.to_string().len() as f32 * 4.0,
                rect.center().y,
            ),
            vec2(34.0, 20.0),
        );
        let _ = chip;
        let label = format!("{n} 项");
        let gal = p.layout_no_wrap(label.clone(), lib_font(11.0), palette.secondary);
        let w = gal.size().x + 16.0;
        let chip = Rect::from_min_size(
            pos2(
                rect.left()
                    + p.layout_no_wrap(title.to_string(), lib_font(18.0), palette.text)
                        .size()
                        .x
                    + 10.0,
                rect.center().y - 10.0,
            ),
            vec2(w, 20.0),
        );
        p.rect_filled(chip, CornerRadius::same(10), palette.surface);
        p.galley(
            chip.min + vec2(8.0, (20.0 - gal.size().y) / 2.0),
            gal,
            palette.secondary,
        );
    }

    // 页内提示条（右上，短暂显示）。
    if let Some((msg, at)) = &state.notice {
        let age = at.elapsed().as_secs_f32();
        if age > 3.0 {
            state.notice = None;
        } else {
            let alpha = (1.0 - (age - 2.4).max(0.0) / 0.6).clamp(0.0, 1.0);
            let gal = p.layout_no_wrap(msg.clone(), lib_font(12.0), palette.warning);
            let w = gal.size().x + 20.0;
            let bar = Rect::from_min_size(
                pos2(rect.right() - 300.0 - w, rect.top() + 4.0),
                vec2(w, 26.0),
            );
            p.rect_filled(
                bar,
                CornerRadius::same(13),
                fade(palette.surface_hover, alpha),
            );
            p.galley_with_override_text_color(
                bar.min + vec2(10.0, (26.0 - gal.size().y) / 2.0),
                gal,
                fade(palette.warning, alpha),
            );
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    // 搜索框：胶囊外壳 + 放大镜 glyph + 内嵌 TextEdit。
    let field_w = 220.0;
    let field = Rect::from_min_max(
        pos2(rect.right() - field_w, rect.top() + 1.0),
        pos2(rect.right(), rect.bottom() - 1.0),
    );
    let (focused, dirty) = search_field(ui, field, state, palette);
    let _ = (focused, dirty);
}

/// 自绘搜索胶囊：外壳 + glyph + 透明 TextEdit 内核。
/// 返回 (是否聚焦, 是否修改)。每帧调用以承载编辑状态。
fn search_field(
    ui: &mut Ui,
    rect: Rect,
    state: &mut BrowseUiState,
    palette: Palette,
) -> (bool, bool) {
    let id = Id::unique("lib-search");
    let inner = rect.shrink2(vec2(30.0, 2.0));
    // 垂直居中两步缺一不可（egui 0.37）：
    // 1. cross_justify 把 TextEdit rect 拉满胶囊内高；
    // 2. .align(LEFT_CENTER) 让文字/光标 galley 在 rect 内垂直居中——
    //    默认 align 取 ui.layout().align2()，只居中 rect 不居中 galley，
    //    表现为文字偏上。
    let mut field_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("lib-search-field")
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center).with_cross_justify(true)),
    );
    let te = egui::TextEdit::singleline(&mut state.search)
        .id(id)
        .hint_text("搜索标题…")
        .font(lib_font(12.5))
        .frame(egui::Frame::new())
        .desired_width(f32::INFINITY)
        .align(Align2::LEFT_CENTER);
    let resp = field_ui.add(te);
    let changed = resp.changed();
    // 渲染完成后再画外壳：聚焦态直接取 TextEdit 响应。
    let focused = resp.has_focus();
    let stroke_color = if focused {
        palette.accent
    } else {
        palette.outline
    };
    let p = ui.painter();
    p.rect_stroke(
        rect,
        CornerRadius::same(15),
        Stroke::new(1.2, stroke_color),
        StrokeKind::Inside,
    );
    paint_glyph(
        p,
        Glyph::Search,
        Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), vec2(13.0, 13.0)),
        if focused { palette.accent } else { palette.dim },
    );
    (focused, changed)
}

// ---------------------------------------------------------------------------
// 首页：继续观看 + 媒体源卡片
// ---------------------------------------------------------------------------

/// 首页：继续观看（非媒体库单文件）+ 最近访问的媒体库（播放历史聚合，
/// 媒体库视角的续播）。媒体源的新增/编辑入口在侧栏（+ 与行内编辑钮）。
fn ui_home(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    browser: &BrowserState,
    store: &Arc<Store>,
    palette: Palette,
) {
    // media_lib 先 clone 到局部：拆分结果的引用借局部值而非 state，
    // 后续 media_row / card_grid 闭包可变借用 state 不冲突。
    let media_lib = state.media_lib.clone();
    let (singles, libs) = split_continuing(&browser.recent, &media_lib);
    let lib_folder_paths: Vec<String> = media_lib.iter().map(|e| e.folder_path.clone()).collect();

    // 继续观看：不属于任何媒体库的进行中单文件（拖放/未入库目录）。
    if singles.is_empty() {
        section_header(ui, "继续观看", Some(singles.len()), palette);
        empty_state(
            ui,
            Glyph::History,
            "没有进行中的内容",
            "未加入媒体库的单文件播放会显示在这里，可从上次的位置继续",
            palette,
        );
    } else {
        let clear = section_header_with_clear(ui, "继续观看", singles.len(), palette);
        if clear.clicked() {
            state.confirm_clear_history = Some(ClearHistoryTarget::AllSingles {
                lib_folder_paths: lib_folder_paths.clone(),
            });
        }
        for r in singles {
            media_row(ui, r, sources, browser, palette, state);
        }
    }

    ui.add_space(18.0);

    // 最近访问的媒体库：有进行中内容的媒体库条目（组内最新访问降序）。
    // 与媒体库视图同款卡片（封面/已看集数/进度 + hover 播放/移除）。
    if libs.is_empty() {
        section_header(ui, "最近访问的媒体库", Some(libs.len()), palette);
        empty_state(
            ui,
            Glyph::Grid,
            if state.media_lib.is_empty() {
                "还没有加入媒体库的文件夹"
            } else {
                "媒体库暂无进行中的内容"
            },
            if state.media_lib.is_empty() {
                "浏览媒体源时点 ＋ 将文件夹加入媒体库，续播会按媒体库聚合在这里"
            } else {
                "从媒体库开始播放后，进行中的内容会出现在这里"
            },
            palette,
        );
        return;
    }
    let clear = section_header_with_clear(ui, "最近访问的媒体库", libs.len(), palette);
    if clear.clicked() {
        state.confirm_clear_history = Some(ClearHistoryTarget::AllLibs {
            lib_folder_paths: lib_folder_paths.clone(),
        });
    }
    let total = libs.len();
    card_grid(
        ui,
        ui.available_width(),
        |ui, i| {
            let e = libs[i];
            state.request_lib_stats(e);
            let stats = state.lib_stats.get(&e.id).copied().unwrap_or_default();

            let src = sources.iter().find(|s| s.id == e.source_id);
            let src_name = src.map(|s| s.name.as_str()).unwrap_or("未知源");
            let (card, remove) = medialib_card(
                ui,
                e,
                src_name,
                &stats,
                palette,
                &mut state.cover_cache,
                &mut state.pending_cover_load,
                &mut state.thumb_cache,
                &mut state.pending_thumb_gen,
            );
            if remove.clicked() {
                if let Err(err) = store.remove_media_library(e.id) {
                    tracing::warn!("移除媒体库条目失败: {err}");
                } else {
                    state.notice = Some((
                        format!("已从媒体库移除：{}", e.display_name),
                        Instant::now(),
                    ));
                }
                state.refresh_media_lib(store);
            } else if card.secondary_clicked() {
                state.confirm_clear_history = Some(ClearHistoryTarget::Folder {
                    folder_path: e.folder_path.clone(),
                    display_name: e.display_name.clone(),
                });
            } else if card.clicked() {
                // 文件夹条目整夹开播（组队定位续播集）；单视频条目
                // 按所在目录组队并定位该文件。
                let path = PathBuf::from(&e.folder_path);
                if lib_entry_is_video(e) {
                    let file = path.clone();
                    let dir = file.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    state.shell_actions.push(ShellAction::PlayFile {
                        source_id: e.source_id,
                        dir,
                        file,
                    });
                } else {
                    state.shell_actions.push(ShellAction::PlayFolder {
                        source_id: e.source_id,
                        dir: path,
                    });
                }
                state.notify(format!("正在打开：{}", e.display_name));
            }
            card
        },
        total,
    );
}

// ---------------------------------------------------------------------------
// 媒体库视图：加入的文件夹条目（media_library 表）+ 已看聚合
// ---------------------------------------------------------------------------

/// 目录内约定封面文件名（探测于当前已列出的 entries，零额外 IO）。
pub(crate) const COVER_NAMES: [&str; 4] = ["cover.jpg", "poster.jpg", "folder.jpg", "cover.png"];

pub(crate) fn detect_cover(entries: &[dyplayer_vfs::VfsEntry]) -> Option<String> {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .find(|e| COVER_NAMES.contains(&e.name.to_lowercase().as_str()))
        .map(|e| e.path.to_string_lossy().into_owned())
}

/// 计算 cover fit（填满 + 居中裁剪）的 UV 矩形：图像 (iw,ih) 填入容器 (cw,ch)，
/// 取较大缩放比铺满，裁剪溢出部分居中（CSS object-fit: cover 语义）。
fn cover_fit_uv(iw: f32, ih: f32, cw: f32, ch: f32) -> Rect {
    if iw <= 0.0 || ih <= 0.0 || cw <= 0.0 || ch <= 0.0 {
        return Rect::from_min_size(pos2(0.0, 0.0), vec2(1.0, 1.0));
    }
    let img_ratio = iw / ih;
    let cov_ratio = cw / ch;
    if img_ratio > cov_ratio {
        // 图像更宽：按高度填满，左右裁剪居中。
        let uv_w = cov_ratio / img_ratio;
        Rect::from_min_size(pos2((1.0 - uv_w) / 2.0, 0.0), vec2(uv_w, 1.0))
    } else {
        // 图像更高：按宽度填满，上下裁剪居中。
        let uv_h = img_ratio / cov_ratio;
        Rect::from_min_size(pos2(0.0, (1.0 - uv_h) / 2.0), vec2(1.0, uv_h))
    }
}


/// 单视频条目判定：folder_path 字段承载"文件夹或单个视频文件"两种语义
/// （零 schema 改动），按视频扩展名区分展示与统计路径。
fn lib_entry_is_video(entry: &MediaLibEntry) -> bool {
    dyplayer_vfs::media::is_video_file(std::path::Path::new(&entry.folder_path))
}

/// 媒体库条目集数统计（后台递归 VFS 枚举 + play_history join 的结果，
/// main.rs 帧末 spawn，经 lib_stats 缓存回流）。
/// total = 文件夹（含全部子层级）内视频总数；watching/finished 只计
/// 库内仍存在的媒体（已删文件的陈旧记录不计入）；watching_frac_sum
/// 为在看各集的进度和（每集 0..1），供整体进度条换算。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EpisodeStats {
    pub total: usize,
    pub watching: usize,
    pub finished: usize,
    pub watching_frac_sum: f32,
}

/// 卡片副行语义（用户定稿）："x 集在看"；**全部看完才是"已看"**；
/// 部分看完显示"已看 x/y 集"；无记录多集 = "未观看" / 单视频 = "单集视频"。
fn lib_sub_label(stats: &EpisodeStats) -> String {
    if stats.total == 0 {
        return "未观看".into();
    }
    if stats.watching > 0 {
        return if stats.total == 1 {
            "在看".into()
        } else {
            format!("{} 集在看", stats.watching)
        };
    }
    if stats.finished >= stats.total {
        return "已看".into();
    }
    if stats.finished > 0 {
        return format!("已看 {}/{} 集", stats.finished, stats.total);
    }
    if stats.total == 1 {
        "单集视频".into()
    } else {
        "未观看".into()
    }
}

/// 整体观看进度 =（看完集数 + 在看集进度和）/ 总集数。
fn lib_progress(stats: &EpisodeStats) -> f32 {
    if stats.total == 0 {
        return 0.0;
    }
    ((stats.finished as f32 + stats.watching_frac_sum) / stats.total as f32).clamp(0.0, 1.0)
}

/// 播放记录归属的媒体库条目（单视频条目精确匹配 / 文件夹条目前缀边界
/// 匹配；分隔符归一 `/`，防 `A` 误配 `AB`）。None = 不属于任何媒体库
/// （首页"继续观看"单文件部分）。
fn media_lib_owner<'a>(
    media_lib: &'a [MediaLibEntry],
    source_id: i64,
    path: &str,
) -> Option<&'a MediaLibEntry> {
    let norm = path.replace('\\', "/");
    media_lib.iter().find(|m| {
        m.source_id == source_id && {
            let fp = m.folder_path.replace('\\', "/");
            if lib_entry_is_video(m) {
                norm == fp
            } else {
                norm.starts_with(&format!("{fp}/"))
            }
        }
    })
}

/// 首页"继续观看"拆分：(单文件部分, 媒体库视角聚合)。
/// 进行中 = `!finished && 进度 > 1%`（与原列表口径一致）；两部分合计 =
/// 原全部进行中内容。单文件保持原顺序（上限 6 行）；聚合按组内最新
/// 访问时间降序（上限 6 卡）。
fn split_continuing<'a>(
    recent: &'a [RecentEntry],
    media_lib: &'a [MediaLibEntry],
) -> (Vec<&'a RecentEntry>, Vec<&'a MediaLibEntry>) {
    let mut singles: Vec<&RecentEntry> = Vec::new();
    let mut grouped: Vec<(&MediaLibEntry, &str)> = Vec::new();
    for r in recent.iter() {
        if r.finished || progress_frac(r.position_sec, r.duration_sec) <= 0.01 {
            continue;
        }
        match media_lib_owner(media_lib, r.source_id, &r.path) {
            Some(e) => {
                if let Some(slot) = grouped.iter_mut().find(|(g, _)| g.id == e.id) {
                    if r.updated_at.as_str() > slot.1 {
                        slot.1 = r.updated_at.as_str();
                    }
                } else {
                    grouped.push((e, r.updated_at.as_str()));
                }
            }
            None => singles.push(r),
        }
    }
    singles.truncate(6);
    grouped.sort_by(|a, b| b.1.cmp(a.1));
    let libs: Vec<&MediaLibEntry> = grouped.into_iter().map(|(e, _)| e).take(6).collect();
    (singles, libs)
}

fn ui_medialib(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],

    store: &Arc<Store>,
    palette: Palette,
) {
    let q = state.search.to_lowercase();
    let entries: Vec<MediaLibEntry> = state
        .media_lib
        .iter()
        .filter(|e| q.is_empty() || e.display_name.to_lowercase().contains(&q))
        .cloned()
        .collect();
    if entries.is_empty() {
        empty_state(
            ui,
            Glyph::Grid,
            "媒体库还是空的",
            if q.is_empty() {
                "在媒体源中浏览，点 ＋ 将文件夹加入媒体库"
            } else {
                "没有匹配的条目"
            },
            palette,
        );
        return;
    }
    let total = entries.len();
    card_grid(
        ui,
        ui.available_width(),
        |ui, i| {
            let e = &entries[i];
            state.request_lib_stats(e);
            let stats = state.lib_stats.get(&e.id).copied().unwrap_or_default();
            let src = sources.iter().find(|s| s.id == e.source_id);
            let src_name = src.map(|s| s.name.as_str()).unwrap_or("未知源");
            let (card, remove) = medialib_card(
                ui,
                e,
                src_name,
                &stats,
                palette,
                &mut state.cover_cache,
                &mut state.pending_cover_load,
                &mut state.thumb_cache,
                &mut state.pending_thumb_gen,
            );
            if remove.clicked() {
                if let Err(err) = store.remove_media_library(e.id) {
                    tracing::warn!("移除媒体库条目失败: {err}");
                } else {
                    state.notice = Some((
                        format!("已从媒体库移除：{}", e.display_name),
                        Instant::now(),
                    ));
                }
                state.refresh_media_lib(store);
            } else if card.clicked() {
                // 文件夹条目整夹开播（组队定位续播集）；单视频条目
                // 按所在目录组队并定位该文件。
                let path = PathBuf::from(&e.folder_path);
                if lib_entry_is_video(e) {
                    let file = path.clone();
                    let dir = file.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    state.shell_actions.push(ShellAction::PlayFile {
                        source_id: e.source_id,
                        dir,
                        file,
                    });
                } else {
                    state.shell_actions.push(ShellAction::PlayFolder {
                        source_id: e.source_id,
                        dir: path,
                    });
                }
                state.notify(format!("正在打开：{}", e.display_name));
            }
            card
        },
        total,
    );
}

/// 媒体库条目卡片：真实封面（本地源 file:// 直显）或几何占位 +
/// 信息条（显示名/源名·集数在看语义）+ 进度细条 + hover 浮起/播放钮/移除钮。
#[allow(clippy::too_many_arguments)]
fn medialib_card(
    ui: &mut Ui,
    entry: &MediaLibEntry,
    src_name: &str,
    stats: &EpisodeStats,
    palette: Palette,
    cover_cache: &mut std::collections::HashMap<String, CoverState>,
    pending_cover_load: &mut Vec<(String, i64)>,
    thumb_cache: &mut std::collections::HashMap<String, CoverState>,
    pending_thumb_gen: &mut Vec<(i64, String)>,
) -> (Response, Response) {
    let id = Id::unique("lib-medialib").with(entry.id);
    let (rect, resp) = ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(id.with("h"), resp.hovered() as u8 as f32, 0.1);
    let lift = 2.0 * t;
    let p = ui.painter().clone();

    if t > 0.01 {
        for (dy, a) in [(5.0f32, 0.28f32), (3.0, 0.18)] {
            p.rect_filled(
                rect.translate(vec2(0.0, dy)),
                CornerRadius::same(12),
                fade(palette.window, a * t),
            );
        }
    }
    let body = rect.translate(vec2(0.0, -lift));
    p.rect_filled(body, CornerRadius::same(12), palette.surface);
    if t > 0.01 {
        p.rect_stroke(
            body.shrink(0.5),
            CornerRadius::same(12),
            Stroke::new(1.2, fade(palette.accent, 0.55 * t)),
            StrokeKind::Inside,
        );
    }

    // 封面区：底色 → 本地约定封面 file:// 直显，否则几何占位（Movie + 首字）。
    let cover = Rect::from_min_max(
        pos2(body.left(), body.top()),
        pos2(body.right(), body.top() + 108.0),
    );
    let corner_top = CornerRadius {
        nw: 12,
        ne: 12,
        sw: 0,
        se: 0,
    };
    p.rect_filled(cover, corner_top, palette.surface_active);
    // 封面：三级来源（成本递增）——
    // 1. 约定封面 cover.jpg（VFS read_full + image 解码 → cover_cache）
    // 2. mpv headless 抽帧（thumb_gen → thumb_cache）
    // 3. 几何占位（glyph + 首字）
    let cover_path = entry.cover_path.as_deref();
    let cover_tex = cover_path.and_then(|c| match cover_cache.get(c) {
        Some(CoverState::Ready(tex)) => Some(egui::TextureHandle::clone(tex)),
        _ => None,
    });
    let thumb_key = if cover_path.is_none() && entry.cover_probed {
        Some(crate::thumb_gen::thumb_key(entry.source_id, &entry.folder_path))
    } else {
        None
    };
    let thumb_tex = thumb_key.as_ref().and_then(|k| match thumb_cache.get(k) {
        Some(CoverState::Ready(tex)) => Some(egui::TextureHandle::clone(tex)),
        _ => None,
    });
    if let Some(tex) = cover_tex.or(thumb_tex) {
        let cov = cover.size();
        let ts = tex.size();
        let uv = cover_fit_uv(ts[0] as f32, ts[1] as f32, cov.x, cov.y);
        ui.put(
            cover,
            egui::Image::from_texture(&tex)
                .fit_to_exact_size(cov)
                .maintain_aspect_ratio(false)
                .uv(uv)
                .corner_radius(corner_top),
        );
    } else {
        // cache miss → 入队后台加载（Failed 不重试）。
        if let Some(c) = cover_path
            && cover_cache.get(c).is_none()
            && !pending_cover_load.iter().any(|(p, _)| p == c)
        {
            pending_cover_load.push((c.to_string(), entry.source_id));
        }
        if let Some(k) = &thumb_key
            && thumb_cache.get(k).is_none()
            && !pending_thumb_gen
                .iter()
                .any(|(sid, fp)| *sid == entry.source_id && fp == &entry.folder_path)
        {
            pending_thumb_gen.push((entry.source_id, entry.folder_path.clone()));
        }
        // 几何占位：Movie/Folder glyph + 首字。
        paint_glyph(
            &p,
            if lib_entry_is_video(entry) {
                Glyph::Movie
            } else {
                Glyph::Folder
            },
            Rect::from_center_size(cover.center(), vec2(26.0, 26.0)),
            fade(palette.dim, 0.9),
        );
        let initial = entry.display_name.chars().next().unwrap_or('?');
        p.text(
            pos2(cover.center().x, cover.center().y + 25.0),
            Align2::CENTER_CENTER,
            initial.to_string(),
            lib_font(13.0),
            fade(palette.secondary, 0.7),
        );
    }

    // hover 播放圆（封面右下，淡入放大）。
    if t > 0.01 {
        let c = pos2(body.right() - 22.0, body.top() + 108.0);
        p.circle_filled(c, 14.0 + 3.0 * t, palette.accent);
        paint_glyph(
            &p,
            Glyph::Play,
            Rect::from_center_size(c, vec2(13.0, 13.0)),
            palette.on_accent,
        );
    }

    // 信息条：显示名 + 源名·已看集数。
    let info_top = body.top() + 108.0;
    p.text(
        pos2(body.left() + 10.0, info_top + 14.0),
        Align2::LEFT_CENTER,
        truncate_tail(&entry.display_name, 13),
        lib_font(12.0),
        palette.text,
    );
    let sub = format!("{src_name} · {}", lib_sub_label(stats));
    p.text(
        pos2(body.left() + 10.0, info_top + 31.0),
        Align2::LEFT_CENTER,
        truncate_tail(&sub, 20),
        lib_font(10.5),
        palette.dim,
    );

    // 进度细条：(看完集数 + 在看集进度和) / 总集数。
    // 居于封面底/信息条顶分界线（info_top），作自然分割；避开卡片底圆角。
    let progress = lib_progress(stats);
    if progress > 0.005 {
        let w = body.width() * progress.clamp(0.0, 1.0);
        p.rect_filled(
            Rect::from_min_max(
                pos2(body.left(), info_top - 1.5),
                pos2(body.left() + w, info_top + 1.5),
            ),
            CornerRadius::ZERO,
            palette.accent,
        );
    }

    // 移除 ×（hover 时右上角浮现，后注册赢点击）。
    let x_rect = Rect::from_center_size(
        pos2(body.right() - 16.0, body.top() + 16.0),
        vec2(20.0, 20.0),
    );
    let x_resp = ui.interact(x_rect, id.with("rm"), Sense::click());
    if resp.hovered() || x_resp.hovered() {
        let xt = ui.ctx().animate_value_with_time(id.with("xt"), 1.0, 0.08);
        p.circle_filled(x_rect.center(), 10.0, fade(palette.window, 0.92 * xt));
        p.line_segment(
            [
                pos2(x_rect.center().x - 3.5, x_rect.center().y - 3.5),
                pos2(x_rect.center().x + 3.5, x_rect.center().y + 3.5),
            ],
            Stroke::new(1.4, palette.secondary),
        );
        p.line_segment(
            [
                pos2(x_rect.center().x - 3.5, x_rect.center().y + 3.5),
                pos2(x_rect.center().x + 3.5, x_rect.center().y - 3.5),
            ],
            Stroke::new(1.4, palette.secondary),
        );
    }
    let x_resp = x_resp.on_hover_text("从媒体库移除");
    (resp, x_resp)
}


// ---------------------------------------------------------------------------
// 媒体源视图：文件夹卡片（海报墙原型，几何占位封面）
// ---------------------------------------------------------------------------

fn ui_library(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    browser: &BrowserState,
    store: &Arc<Store>,
    palette: Palette,
) {
    let Some(idx) = state.selected_source else {
        empty_state(
            ui,
            Glyph::Folder,
            "未选择媒体源",
            "在左侧选择一个源，展示其目录里的文件夹与视频",
            palette,
        );
        return;
    };
    let Some(source) = sources.get(idx) else {
        empty_state(ui, Glyph::Folder, "该源已不存在", "", palette);
        return;
    };


    let q = state.search.to_lowercase();
    let items: Vec<&dyplayer_vfs::VfsEntry> = browser
        .entries
        .iter()
        .filter(|e| e.is_dir || dyplayer_vfs::media::is_video_file(std::path::Path::new(&e.name)))
        .filter(|e| q.is_empty() || e.name.to_lowercase().contains(&q))
        .collect();

    // 媒体索引子目录搜索（媒体源视图专属）：搜索词非空时 = 本列表筛选
    // + 索引下级查询；索引未建立（该源 0 行）→ 空结果自然回退"仅本列表
    // 筛选"。查询按（搜索词, 源）变化触发，帧间复用缓存（SQLite LIKE
    // 万行毫秒级，键入一查非每帧）。其他视图搜索维持各自列表筛选。
    if q.is_empty() {
        state.index_hits.clear();
        state.index_hits_query.clear();
    } else if state.index_hits_query != q || state.index_hits_source != source.id {
        state.index_hits = store
            .search_media_index(source.id, &q, 200)
            .unwrap_or_default();
        state.index_hits_query = q.clone();
        state.index_hits_source = source.id;
    }
    // 去重：当前目录列表已展示的条目不再重复出现。
    // clone 到局部：后续 card_grid 闭包可变借用 state 不与缓存借用冲突。
    let index_hits: Vec<dyplayer_store::IndexHit> = state
        .index_hits
        .iter()
        .filter(|h| {
            let norm = h.path.replace('\\', "/");
            !items
                .iter()
                .any(|e| e.path.to_string_lossy().replace('\\', "/") == norm)
        })
        .cloned()
        .collect();
    // R12：缓存 index_ready，避免每帧查 DB（5 秒或源切换时刷新）。
    let index_ready = match state.index_ready_cache {
        Some((sid, ready, ts)) if sid == source.id && ts.elapsed() < Duration::from_secs(5) => ready,
        _ => {
            let ready = store.media_index_count(source.id).unwrap_or(0) > 0;
            state.index_ready_cache = Some((source.id, ready, Instant::now()));
            ready
        }
    };

    // 面包屑：源名（点击回根）+ 逐级目录段（点击跳该层）；当前段高亮不可点。
    // 固定行高占位 + 垂直居中（绝对坐标画法不占布局高度，后渲染的卡片
    // 会把它盖住）。行高 30 = 可点击导航控件的充足热区。
    let (crumb, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    {
        let p = ui.painter();
        let cy = crumb.center().y;
        let mut x = crumb.left();
        let root = state.source_root.clone();
        let rel: Vec<String> = root
            .as_ref()
            .and_then(|r| browser.dir.strip_prefix(r).ok())
            .map(|rel| {
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        // 段列表：(完整路径, 显示名)；首段是源名（回根）。
        let mut segs: Vec<(Option<PathBuf>, String)> = vec![(None, source.name.clone())];
        let mut acc = root.clone().unwrap_or_default();
        for name in &rel {
            acc = acc.join(name);
            segs.push((Some(acc.clone()), name.clone()));
        }
        for (i, (seg_dir, name)) in segs.iter().enumerate() {
            let is_last = i + 1 == segs.len();
            let gal = p.layout_no_wrap(name.clone(), lib_font(13.0), palette.secondary);
            let w = gal.size().x;
            if !is_last {
                let hit = Rect::from_min_size(
                    pos2(x, crumb.top()),
                    vec2(w.max(24.0), crumb.height()),
                );
                let r = ui.interact(
                    hit,
                    Id::unique("lib-crumb").with(seg_dir),
                    Sense::click(),
                );
                let color = if r.hovered() {
                    palette.accent
                } else {
                    palette.secondary
                };
                p.galley_with_override_text_color(
                    pos2(x, cy - gal.size().y / 2.0),
                    gal,
                    color,
                );
                if r.clicked() {
                    match seg_dir {
                        Some(dir) => state
                            .shell_actions
                            .push(ShellAction::BrowseInto { dir: dir.clone() }),
                        None => state.shell_actions.push(ShellAction::BrowseRoot),
                    }
                }
                r.on_hover_cursor(egui::CursorIcon::PointingHand);
            } else {
                p.galley_with_override_text_color(
                    pos2(x, cy - gal.size().y / 2.0),
                    gal,
                    palette.text,
                );
            }
            x += w;
            if !is_last {
                let sep = p.layout_no_wrap("  ›  ".into(), lib_font(13.0), palette.dim);
                let sep_w = sep.size().x;
                p.galley_with_override_text_color(
                    pos2(x, cy - sep.size().y / 2.0),
                    sep,
                    palette.dim,
                );
                x += sep_w;
            }
        }
    }
    ui.add_space(6.0);

    if items.is_empty() && index_hits.is_empty() {
        if browser.is_loading() {
            empty_state(ui, Glyph::Folder, "正在加载目录…", "", palette);
        } else if !q.is_empty() {
            empty_state(
                ui,
                Glyph::Search,
                "当前目录没有匹配项",
                if index_ready {
                    "子目录索引中也没有匹配项"
                } else {
                    "索引尚未建立，仅筛选当前目录（稍后自动建立）"
                },
                palette,
            );
        } else if browser.entries.is_empty() {
            empty_state(
                ui,
                Glyph::Folder,
                "该目录还没有浏览缓存",
                "进入目录后会自动加载内容",
                palette,
            );
        } else {
            empty_state(ui, Glyph::Folder, "当前目录没有文件夹或视频", "", palette);
        }
        return;
    }
    if !items.is_empty() {
        card_grid(
        ui,
        ui.available_width(),
        |ui, i| {
            let e = items[i];
            let folder_path = e.path.to_string_lossy().into_owned();
            let resp = if e.is_dir {
                let tk = crate::thumb_gen::thumb_key(source.id, &folder_path);
                let thumb = match state.thumb_cache.get(&tk) {
                    Some(CoverState::Ready(tex)) => Some(egui::TextureHandle::clone(tex)),
                    _ => None,
                };
                if thumb.is_none()
                    && !state.thumb_cache.contains_key(&tk)
                    && !state
                        .pending_thumb_gen
                        .iter()
                        .any(|(sid, fp)| *sid == source.id && fp == &folder_path)
                {
                    state.pending_thumb_gen.push((source.id, folder_path.clone()));
                }
                let (r, play) = folder_card(ui, &e.name, "", palette, thumb.as_ref());
                // 播放钮：开播该文件夹（只检测第一层视频，不递归；
                // 无视频时后台回流 FolderPlayFailed → notice 提示）。
                if play.clicked() {
                    state.shell_actions.push(ShellAction::PlayFolder {
                        source_id: source.id,
                        dir: e.path.clone(),
                    });
                    state.notify(format!("正在打开：{}", e.name));
                }
                if r.clicked() {
                    state.shell_actions.push(ShellAction::BrowseInto {
                        dir: e.path.clone(),
                    });
                }
                r.on_hover_cursor(egui::CursorIcon::PointingHand)
            } else {
                // 媒体库是文件夹收藏模型：视频文件不渲染"加入媒体库"
                // 入口（连 ✓ 一起不显示），从渲染层阻断视频被加为媒体，
                // 仅文件夹可加入。
                let tk = crate::thumb_gen::thumb_key(source.id, &folder_path);
                let thumb = match state.thumb_cache.get(&tk) {
                    Some(CoverState::Ready(tex)) => Some(egui::TextureHandle::clone(tex)),
                    _ => None,
                };
                if thumb.is_none()
                    && !state.thumb_cache.contains_key(&tk)
                    && !state
                        .pending_thumb_gen
                        .iter()
                        .any(|(sid, fp)| *sid == source.id && fp == &folder_path)
                {
                    state.pending_thumb_gen.push((source.id, folder_path.clone()));
                }
                let (r, play) = card(
                    ui,
                    Id::unique("lib-card-vid").with(&folder_path),
                    &e.name,
                    &human_size(e.size),
                    Glyph::Movie,
                    palette,
                    thumb.as_ref(),
                );
                // 播放钮与整卡同语义（开播该文件）。
                if r.clicked() || play.clicked() {
                    state.shell_actions.push(ShellAction::PlayFile {
                        source_id: source.id,
                        dir: browser.dir.clone(),
                        file: e.path.clone(),
                    });
                    state.notify(format!("正在打开：{}", e.name));
                }
                return r.on_hover_cursor(egui::CursorIcon::PointingHand);
            };
            let in_lib = state
                .media_lib
                .iter()
                .any(|m| m.source_id == source.id && m.folder_path == folder_path);
            // ／加入媒体库（hover 右上角浮现；已加入显示 ✓）。
            let plus_rect = Rect::from_center_size(
                pos2(resp.rect.right() - 18.0, resp.rect.top() + 16.0),
                vec2(20.0, 20.0),
            );
            let plus = ui.interact(
                plus_rect,
                Id::unique("lib-add").with(&folder_path),
                Sense::click(),
            );
            if resp.hovered() || plus.hovered() {
                let p = ui.painter();
                p.circle_filled(plus_rect.center(), 10.0, fade(palette.window, 0.92));
                if in_lib {
                    paint_glyph(
                        p,
                        Glyph::Check,
                        Rect::from_center_size(plus_rect.center(), vec2(11.0, 11.0)),
                        palette.accent,
                    );
                } else {
                    paint_glyph(
                        p,
                        Glyph::Plus,
                        Rect::from_center_size(plus_rect.center(), vec2(11.0, 11.0)),
                        palette.secondary,
                    );
                }
            }
            if plus.clicked() && !in_lib {
                // 异步加入：后台 list_dir 子文件夹探测 cover.jpg 约定封面
                //（detect_cover 需未过滤 entries，request_dir 的 is_listable_entry
                // 已滤掉 cover.jpg，故必须独立 list_dir），main.rs 帧末 spawn。
                state.shell_actions.push(ShellAction::AddToLib {
                    source_id: source.id,
                    folder_path: e.path.clone(),
                    name: e.name.to_string(),
                });
                state.notice = Some((format!("正在加入媒体库：{}", e.name), Instant::now()));
            }
            plus.on_hover_text(if in_lib {
                "已在媒体库"
            } else {
                "加入媒体库（自动识别 cover.jpg 等封面）"
            });
            resp
        },
        items.len(),
    );
    }

    // 子目录匹配（媒体索引）：搜索词命中的下级视频，sub 显示相对源根
    // 的父目录路径；点击按所在目录组队开播（PlayFile 定位该文件）。
    if !index_hits.is_empty() {
        ui.add_space(12.0);
        section_header(ui, "子目录匹配", Some(index_hits.len()), palette);
        let root = state.source_root.clone();
        let hits = index_hits;
        card_grid(
            ui,
            ui.available_width(),
            |ui, i| {
                let h = &hits[i];
                let file = PathBuf::from(&h.path);
                let dir = file.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                // 相对源根的父目录（无根/越界回退完整父目录字符串）。
                let rel_dir = root
                    .as_ref()
                    .and_then(|r| dir.strip_prefix(r).ok())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| h.path.clone());
                let (r, play) = card(
                    ui,
                    Id::unique("lib-card-idx").with(&h.path),
                    &h.name,
                    &truncate_tail(&rel_dir, 18),
                    Glyph::Movie,
                    palette,
                    None,
                );
                if r.clicked() || play.clicked() {
                    state.shell_actions.push(ShellAction::PlayFile {
                        source_id: source.id,
                        dir,
                        file,
                    });
                    state.notify(format!("正在打开：{}", h.name));
                }
                r.on_hover_cursor(egui::CursorIcon::PointingHand)
            },
            hits.len(),
        );
    } else if !q.is_empty() && !index_ready {
        // 索引未建立的明确提示（搜索词非空时）：回退语义对用户可见。
        ui.add_space(10.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
        ui.painter().text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            "索引尚未建立，仅筛选当前目录；媒体索引会在应用空闲时自动建立",
            lib_font(11.0),
            palette.dim,
        );
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f32;
    let mut u = 0usize;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

// ---------------------------------------------------------------------------
// 最近播放 / 收藏视图（真实数据富信息行）
// ---------------------------------------------------------------------------

fn ui_recent(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    browser: &BrowserState,
    palette: Palette,
) {
    let q = state.search.to_lowercase();
    let rows: Vec<&RecentEntry> = browser
        .recent
        .iter()
        .filter(|r| q.is_empty() || r.title.to_lowercase().contains(&q))
        .collect();
    if rows.is_empty() {
        empty_state(
            ui,
            Glyph::History,
            "暂无播放记录",
            if q.is_empty() {
                ""
            } else {
                "没有匹配的记录"
            },
            palette,
        );
        return;
    }
    let clear = section_header_with_clear(ui, "最近播放", rows.len(), palette);
    if clear.clicked() {
        state.confirm_clear_history = Some(ClearHistoryTarget::All);
    }
    ui.add_space(4.0);
    egui::ScrollArea::vertical().show(ui, |ui| {
        for r in rows {
            media_row(ui, r, sources, browser, palette, state);
        }
    });
}

fn ui_favorites(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    sources: &[MediaSource],
    browser: &BrowserState,
    palette: Palette,
) {
    let q = state.search.to_lowercase();
    let rows: Vec<&FavoriteEntry> = browser
        .favorites
        .iter()
        .filter(|f| q.is_empty() || f.title.to_lowercase().contains(&q))
        .collect();
    if rows.is_empty() {
        empty_state(
            ui,
            Glyph::Star,
            "暂无收藏",
            if q.is_empty() {
                "浏览文件时点 ☆ 收藏"
            } else {
                "没有匹配的收藏"
            },
            palette,
        );
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for f in rows {
            fav_row(ui, f, sources, palette, state);
        }
    });
}

// ---------------------------------------------------------------------------
// 行组件（整行可点 + 绝对坐标列 + 底部进度条）
// ---------------------------------------------------------------------------

fn media_row(
    ui: &mut Ui,
    r: &RecentEntry,
    sources: &[MediaSource],
    browser: &BrowserState,
    palette: Palette,
    state: &mut BrowseUiState,
) {
    let row_h = 56.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    // 动画 Id 必须带 source_id：同一文件的拖放记录与源内播放记录 path 相同，
    // 只用 path 会让两条记录共享 hover 动画值（一条 hover 两条同时亮）。
    let t = ui.ctx().animate_value_with_time(
        Id::unique("lib-row").with((r.source_id, &r.path)),
        resp.hovered() as u8 as f32,
        0.09,
    );
    let p = ui.painter();

    if t > 0.01 {
        p.rect_filled(
            rect,
            CornerRadius::same(9),
            mix(palette.panel, palette.surface_hover, t),
        );
    }

    // 左：状态 glyph（看完 = 对勾 / 进行中 = 播放），垂直居中于整行。
    // hover 时该位置被 accent 播放圆覆盖（动态替换动效）。
    let done = r.finished;
    let icon_c = pos2(rect.left() + 24.0, rect.center().y);
    if t < 0.99 {
        paint_glyph(
            p,
            if done { Glyph::Check } else { Glyph::Play },
            Rect::from_center_size(icon_c, vec2(15.0, 15.0)),
            if done {
                fade(palette.dim, 1.0 - t)
            } else {
                fade(palette.accent, 1.0 - t)
            },
        );
    }

    // 标题 + 源徽章（第一行）。
    let title_x = rect.left() + 46.0;
    let badge = source_label(sources, r.source_id);
    let title_max_w = rect.width() - 46.0 - 150.0;
    let [meta_top, meta_sub] = meta_anchors(rect, title_max_w);
    paint_left_center_truncated(
        p,
        pos2(title_x, rect.top() + 17.0),
        &r.title,
        lib_font(13.5),
        palette.text,
        title_max_w,
    );
    p.text(
        meta_top,
        Align2::LEFT_CENTER,
        badge,
        lib_font(11.0),
        palette.dim,
    );

    // 副行：进度时间。
    let frac = progress_frac(r.position_sec, r.duration_sec);
    let sub = if r.duration_sec > 0.0 {
        format!(
            "{} / {} · 剩余 {}",
            fmt_time(r.position_sec),
            fmt_time(r.duration_sec),
            fmt_time((r.duration_sec - r.position_sec).max(0.0)),
        )
    } else {
        fmt_time(r.position_sec)
    };
    p.text(
        pos2(title_x, rect.top() + 35.0),
        Align2::LEFT_CENTER,
        sub,
        lib_font(11.5),
        palette.secondary,
    );

    // 右：收藏星标 + 更新时间 + hover 播放钮浮现。
    let star_c = pos2(rect.right() - 24.0, rect.center().y);
    let star_r = ui.interact(
        Rect::from_center_size(star_c, vec2(22.0, 22.0)),
        Id::unique("lib-row-star").with((r.source_id, &r.path)),
        Sense::click(),
    );
    let fav = browser
        .favorites
        .iter()
        .any(|f| f.source_id == r.source_id && f.path == r.path);
    paint_glyph(
        p,
        Glyph::Star,
        Rect::from_center_size(star_c, vec2(14.0, 14.0)),
        if fav || star_r.hovered() {
            palette.accent
        } else {
            fade(palette.dim, 0.75)
        },
    );
    if star_r.clicked() {
        state.shell_actions.push(ShellAction::ToggleFavorite {
            source_id: r.source_id,
            path: PathBuf::from(&r.path),
            title: r.title.clone(),
            is_dir: false,
        });
    }
    star_r
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if fav { "取消收藏" } else { "收藏" });
    p.text(
        meta_sub,
        Align2::LEFT_CENTER,
        short_stamp(&r.updated_at),
        lib_font(11.0),
        palette.dim,
    );
    if t > 0.01 {
        // hover 播放圆浮现在左侧原状态图标处（覆盖式动效）。
        p.circle_filled(
            icon_c,
            15.0,
            fade(mix(palette.accent, palette.accent_hover, t), t),
        );
        paint_glyph(
            p,
            Glyph::Play,
            Rect::from_center_size(icon_c, vec2(13.0, 13.0)),
            fade(palette.on_accent, t),
        );
    }

    // 底部进度条（DyPlayer 独有卖点：观看进度可视化）。
    let track = Rect::from_min_max(
        pos2(rect.left() + 10.0, rect.bottom() - 2.5),
        pos2(rect.right() - 10.0, rect.bottom()),
    );
    p.rect_filled(track, CornerRadius::same(1), fade(palette.outline, 0.8));
    if frac > 0.0 {
        let w = track.width() * if done { 1.0 } else { frac };
        p.rect_filled(
            Rect::from_min_size(track.min, vec2(w, track.height())),
            CornerRadius::same(1),
            if done {
                fade(palette.accent, 0.45)
            } else {
                palette.accent
            },
        );
    }

    if resp.clicked() {
        state.shell_actions.push(ShellAction::ResumeRecent {
            source_id: r.source_id,
            path: PathBuf::from(&r.path),
            http_headers: r.http_headers.clone(),
        });
        state.notify(format!("正在打开：{}", r.title));
    }
    if resp.secondary_clicked() {
        state.confirm_clear_history = Some(ClearHistoryTarget::Single {
            source_id: r.source_id,
            path: r.path.clone(),
            title: r.title.clone(),
        });
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("左键播放 · 右键清除记录");
}

fn fav_row(
    ui: &mut Ui,
    f: &FavoriteEntry,
    sources: &[MediaSource],
    palette: Palette,
    state: &mut BrowseUiState,
) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
    let t = ui.ctx().animate_value_with_time(
        Id::unique("lib-fav").with(&(f.source_id, f.path.clone())),
        resp.hovered() as u8 as f32,
        0.09,
    );
    let p = ui.painter();
    if t > 0.01 {
        p.rect_filled(
            rect,
            CornerRadius::same(9),
            mix(palette.panel, palette.surface_hover, t),
        );
    }

    let star_c = pos2(rect.left() + 22.0, rect.center().y);
    if t < 0.99 {
        paint_glyph(
            p,
            Glyph::Star,
            Rect::from_center_size(star_c, vec2(16.0, 16.0)),
            fade(palette.warning, 1.0 - t),
        );
    }
    if t > 0.01 {
        // hover 播放圆覆盖星标位置（与最近播放置换动效一致）。
        p.circle_filled(
            star_c,
            15.0,
            fade(mix(palette.accent, palette.accent_hover, t), t),
        );
        paint_glyph(
            p,
            Glyph::Play,
            Rect::from_center_size(star_c, vec2(13.0, 13.0)),
            fade(palette.on_accent, t),
        );
    }
    paint_glyph(
        p,
        if f.is_dir {
            Glyph::Folder
        } else {
            Glyph::Movie
        },
        Rect::from_center_size(pos2(rect.left() + 46.0, rect.center().y), vec2(15.0, 15.0)),
        palette.secondary,
    );

    let title_max_w = rect.width() - 70.0 - 170.0;
    paint_left_center_truncated(
        p,
        pos2(rect.left() + 62.0, rect.center().y),
        &f.title,
        lib_font(13.0),
        palette.text,
        title_max_w,
    );
    p.text(
        pos2(rect.left() + 66.0 + title_max_w, rect.center().y),
        Align2::LEFT_CENTER,
        format!(
            "{} · {}",
            source_label(sources, f.source_id),
            if f.is_dir { "文件夹" } else { "文件" }
        ),
        lib_font(11.0),
        palette.dim,
    );

    // 右：取消收藏星标（hover 变 danger 提示将移除）。
    let star_c = pos2(rect.right() - 24.0, rect.center().y);
    let star_r = ui.interact(
        Rect::from_center_size(star_c, vec2(22.0, 22.0)),
        Id::unique("lib-fav-star").with((f.source_id, &f.path)),
        Sense::click(),
    );
    paint_glyph(
        p,
        Glyph::Star,
        Rect::from_center_size(star_c, vec2(14.0, 14.0)),
        if star_r.hovered() {
            palette.danger
        } else {
            palette.accent
        },
    );
    if star_r.clicked() {
        state.shell_actions.push(ShellAction::ToggleFavorite {
            source_id: f.source_id,
            path: PathBuf::from(&f.path),
            title: f.title.clone(),
            is_dir: f.is_dir,
        });
    }
    star_r
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("取消收藏");

    if resp.clicked() {
        if f.is_dir {
            state.shell_actions.push(ShellAction::OpenFolder {
                source_id: f.source_id,
                dir: PathBuf::from(&f.path),
            });
        } else {
            state.shell_actions.push(ShellAction::ResumeRecent {
                source_id: f.source_id,
                path: PathBuf::from(&f.path),
                http_headers: None,
            });
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
}

// ---------------------------------------------------------------------------
// 卡片组件（浮起 + 几何封面 + hover 播放钮）
// ---------------------------------------------------------------------------

const CARD_W: f32 = 168.0;
const CARD_H: f32 = 152.0;
const CARD_GAP: f32 = 12.0;

/// 卡片网格：手动行列游标（Musify library_grid 模式）。
/// `render(ui, index)` 返回卡片响应；index 超出数据长度时由闭包决定
/// （如添加源占位卡）。行数不足一列时逐行排列。
fn card_grid<F>(ui: &mut Ui, avail_w: f32, mut render: F, total: usize)
where
    F: FnMut(&mut Ui, usize) -> Response,
{
    let cols = columns_for_width(avail_w);
    let rows = total.div_ceil(cols).max(1);
    let mut idx = 0;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for _row in 0..rows {
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                for _col in 0..cols {
                    if idx >= total {
                        break;
                    }
                    render(ui, idx);
                    ui.add_space(CARD_GAP);
                    idx += 1;
                }
            });
            ui.add_space(CARD_GAP);
        }
    });
}

fn folder_card(
    ui: &mut Ui,
    name: &str,
    sub: &str,
    palette: Palette,
    thumb: Option<&egui::TextureHandle>,
) -> (Response, Response) {
    card(
        ui,
        Id::unique("lib-card-dir").with(name),
        name,
        sub,
        Glyph::Folder,
        palette,
        thumb,
    )
}

/// 通用卡片：几何封面构图（两块 accent 斜切 + 大 glyph）+ 信息条 +
/// hover 浮起（位移 + 底部阴影）+ hover 播放钮 overlay（真实按钮，返回
/// (整卡, 播放钮) 两个 Response；播放钮后注册赢 click，点击不落整卡）。
/// thumb 非空时封面区显示缩略图（cover fit），否则几何占位。
fn card(
    ui: &mut Ui,
    id: Id,
    title: &str,
    sub: &str,
    glyph: Glyph,
    palette: Palette,
    thumb: Option<&egui::TextureHandle>,
) -> (Response, Response) {
    let (rect, resp) = ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::click());
    // 播放钮热区前置注册（保持 resp→play 注册序：play 后注册赢 click，
    // 点击不落整卡）。中心按未浮起坐标，lift ≤ 2px 对 40×40 热区无感。
    let play = ui.interact(
        Rect::from_center_size(
            pos2(rect.center().x, rect.top() + 48.0),
            vec2(40.0, 40.0),
        ),
        id.with("play"),
        Sense::click(),
    );
    // hover 驱动合并整卡与播放钮：指针移入按钮热区后，整卡 hover 被顶层
    // 播放钮取代（若只看整卡，t 衰减 → 按钮淡出隐藏），合并后持续显示。
    let t = ui.ctx().animate_value_with_time(
        id.with("h"),
        (resp.hovered() || play.hovered()) as u8 as f32,
        0.1,
    );
    let lift = 2.0 * t;
    let p = ui.painter().clone();

    // 阴影（浮起感）。
    if t > 0.01 {
        for (dy, a) in [(5.0f32, 0.28f32), (3.0, 0.18)] {
            p.rect_filled(
                rect.translate(vec2(0.0, dy)),
                CornerRadius::same(12),
                fade(palette.window, a * t),
            );
        }
    }

    // 卡体（surface）+ hover 边框。
    let body = rect.translate(vec2(0.0, -lift));
    p.rect_filled(body, CornerRadius::same(12), palette.surface);
    if t > 0.01 {
        p.rect_stroke(
            body.shrink(0.5),
            CornerRadius::same(12),
            Stroke::new(1.2, fade(palette.accent, 0.55 * t)),
            StrokeKind::Inside,
        );
    }

    // 封面区：缩略图（cover fit）或几何构图（accent 两块斜切 + 大 glyph）。
    let cover = Rect::from_min_max(
        pos2(body.left(), body.top()),
        pos2(body.right(), body.top() + 96.0),
    );
    let corner_top = CornerRadius {
        nw: 12,
        ne: 12,
        sw: 0,
        se: 0,
    };
    if let Some(tex) = thumb {
        let cov = cover.size();
        let ts = tex.size();
        let uv = cover_fit_uv(ts[0] as f32, ts[1] as f32, cov.x, cov.y);
        p.rect_filled(cover, corner_top, palette.surface_active);
        ui.put(
            cover,
            egui::Image::from_texture(tex)
                .fit_to_exact_size(cov)
                .maintain_aspect_ratio(false)
                .uv(uv)
                .corner_radius(corner_top),
        );
    } else {
        p.rect_filled(cover, CornerRadius::same(12), palette.surface_active);
        let c = palette.accent;
        p.add(egui::Shape::convex_polygon(
            vec![
                cover.left_bottom(),
                pos2(cover.left() + cover.width() * 0.55, cover.bottom()),
                cover.left_top(),
            ],
            fade(c, 0.12),
            Stroke::NONE,
        ));
        p.add(egui::Shape::convex_polygon(
            vec![
                cover.right_bottom(),
                pos2(cover.right() - cover.width() * 0.3, cover.bottom()),
                cover.right_top(),
            ],
            fade(c, 0.20),
            Stroke::NONE,
        ));
        paint_glyph(
            &p,
            glyph,
            Rect::from_center_size(cover.center(), vec2(36.0, 36.0)),
            mix(palette.secondary, palette.text, 0.4),
        );
    }

    // hover 播放钮 overlay（热区已在函数头前置注册）。
    let bc = cover.center();
    if t > 0.01 {
        let hp = play.hovered() as u8 as f32;
        p.circle_filled(bc, 26.0 + 2.0 * hp, fade(Color32::BLACK, 0.35 * t));
        p.circle_filled(
            bc,
            19.0 + 2.0 * hp,
            fade(mix(palette.accent, palette.accent_hover, hp), t),
        );
        paint_glyph(
            &p,
            Glyph::Play,
            Rect::from_center_size(bc, vec2(16.0, 16.0)),
            fade(palette.on_accent, t),
        );
    }


    // 信息条。
    let info_top = cover.bottom();
    p.text(
        pos2(body.left() + 10.0, info_top + 17.0),
        Align2::LEFT_CENTER,
        "",
        lib_font(12.5),
        palette.text,
    );
    paint_left_center_truncated(
        &p,
        pos2(body.left() + 10.0, info_top + 17.0),
        title,
        lib_font(12.5),
        palette.text,
        CARD_W - 20.0,
    );
    if !sub.is_empty() {
        p.text(
            pos2(body.left() + 10.0, info_top + 36.0),
            Align2::LEFT_CENTER,
            sub,
            lib_font(11.0),
            palette.dim,
        );
    }

    (
        resp.on_hover_cursor(egui::CursorIcon::PointingHand),
        play.on_hover_cursor(egui::CursorIcon::PointingHand),
    )
}

// ---------------------------------------------------------------------------
// 通用小组件
// ---------------------------------------------------------------------------

fn section_header(ui: &mut Ui, title: &str, count: Option<usize>, palette: Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    let p = ui.painter();
    p.text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        title,
        lib_font(15.0),
        palette.text,
    );
    if let Some(n) = count {
        p.text(
            pos2(
                rect.left()
                    + 8.0
                    + p.layout_no_wrap(title.to_string(), lib_font(15.0), palette.text)
                        .size()
                        .x,
                rect.center().y,
            ),
            Align2::LEFT_CENTER,
            format!("{n}"),
            lib_font(11.5),
            palette.dim,
        );
    }
}

/// section_header + 右对齐"清除全部"文字按钮（hover 变 danger 色）。
/// 返回按钮 Response（clicked = 用户请求清除该区全部记录）。
fn section_header_with_clear(ui: &mut Ui, title: &str, count: usize, palette: Palette) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    let p = ui.painter();
    p.text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        title,
        lib_font(15.0),
        palette.text,
    );
    let title_w = p
        .layout_no_wrap(title.to_string(), lib_font(15.0), palette.text)
        .size()
        .x;
    p.text(
        pos2(rect.left() + 8.0 + title_w, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{count}"),
        lib_font(11.5),
        palette.dim,
    );
    let label = "清除全部";
    let gal = p.layout_no_wrap(label.to_string(), lib_font(11.5), palette.text);
    let btn_w = gal.size().x + 16.0;
    let btn_rect = Rect::from_min_size(
        pos2(rect.right() - btn_w, rect.center().y - 13.0),
        vec2(btn_w, 26.0),
    );
    let id = Id::unique("sec-clear").with(title);
    let resp = ui.interact(btn_rect, id, Sense::click());
    let t = ui
        .ctx()
        .animate_value_with_time(id.with("h"), resp.hovered() as u8 as f32, 0.09);
    p.text(
        btn_rect.center(),
        Align2::CENTER_CENTER,
        label,
        lib_font(11.5),
        if t > 0.01 {
            palette.danger
        } else {
            palette.dim
        },
    );
    resp
}

/// 空状态三件套：大 glyph + 主文案 + 副文案。
fn empty_state(ui: &mut Ui, glyph: Glyph, title: &str, sub: &str, palette: Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 150.0), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    paint_glyph(
        p,
        glyph,
        Rect::from_center_size(pos2(c.x, c.y - 30.0), vec2(40.0, 40.0)),
        palette.dim,
    );
    p.text(
        pos2(c.x, c.y + 10.0),
        Align2::CENTER_CENTER,
        title,
        lib_font(14.0),
        palette.secondary,
    );
    if !sub.is_empty() {
        p.text(
            pos2(c.x, c.y + 32.0),
            Align2::CENTER_CENTER,
            sub,
            lib_font(11.5),
            palette.dim,
        );
    }
}

fn pill_button(ui: &mut Ui, label: &str, palette: Palette, hover_text: Color32) -> Response {
    let gal = ui
        .painter()
        .layout_no_wrap(label.to_string(), lib_font(12.0), palette.secondary);
    let w = gal.size().x + 24.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    let t = ui.ctx().animate_value_with_time(
        Id::unique("lib-pill").with(label),
        resp.hovered() as u8 as f32,
        0.09,
    );
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(14),
        mix(palette.surface, palette.surface_hover, t),
    );
    p.galley_with_override_text_color(
        rect.min + vec2(12.0, (rect.height() - gal.size().y) / 2.0),
        gal,
        mix(palette.secondary, hover_text, t),
    );
    resp
}

fn paint_hline(ui: &mut Ui, palette: Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::ZERO, palette.outline);
}

// ---------------------------------------------------------------------------
// 编辑/添加源 modal（同一定制弹框；保存经 ShellAction::SaveSource 由
// main.rs 帧末落库，source_id None/Some 分派新建/更新）
// ---------------------------------------------------------------------------

fn ui_edit_modal(parent: &mut Ui, state: &mut BrowseUiState, palette: Palette) {
    let Some(form) = state.edit.as_mut() else {
        return;
    };
    let screen = parent.max_rect();
    let ctx = parent.ctx().clone();

    // 独立 Foreground 层：egui 的 hit test 只跨层遮挡（同层后画不挡先画 hover），
    // 遮罩 interact 必须放高层才能真正拦截下层所有指针事件。
    let layer = egui::LayerId::new(egui::Order::Foreground, Id::unique("lib-edit-layer"));
    let mut lui = parent.new_child(
        egui::UiBuilder::new()
            .layer_id(layer)
            .max_rect(screen)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    let p = lui.painter();

    // 遮罩：点击空白关闭（卡片区域另放吸收层，点卡片空白不误关）。
    p.rect_filled(screen, CornerRadius::ZERO, fade(palette.window, 0.62));
    let mask = lui.interact(screen, Id::unique("lib-edit-mask"), Sense::click());
    let close_by_mask = mask.clicked();

    // 居中卡片。
    let size = vec2(430.0, 470.0);
    let card = Rect::from_center_size(screen.center(), size);
    p.rect_filled(card, CornerRadius::same(14), palette.panel);
    p.rect_stroke(
        card.shrink(0.5),
        CornerRadius::same(14),
        Stroke::new(1.0, palette.outline),
        StrokeKind::Inside,
    );
    let _card_hit = lui.interact(card, Id::unique("lib-edit-card-hit"), Sense::click());

    let mut mui = lui.new_child(
        egui::UiBuilder::new()
            .id_salt("lib-edit-card")
            .max_rect(card.shrink2(vec2(24.0, 20.0)))
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    mui.spacing_mut().item_spacing.y = 7.0;

    let is_create = form.source_id.is_none();

    // 标题行：编辑/添加媒体源 + 类型徽章。
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 26.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            if is_create { "添加媒体源" } else { "编辑媒体源" },
            lib_font(15.0),
            palette.text,
        );
        let label = form.kind.label();
        let gal = mp.layout_no_wrap(label.to_owned(), lib_font(11.0), palette.secondary);
        let w = gal.size().x + 16.0;
        let chip = Rect::from_min_size(
            pos2(rect.right() - w, rect.center().y - 10.0),
            vec2(w, 20.0),
        );
        mp.rect_filled(chip, CornerRadius::same(10), palette.surface);
        mp.galley(
            chip.min + vec2(8.0, (20.0 - gal.size().y) / 2.0),
            gal,
            palette.secondary,
        );
    }

    form_field(&mut mui, "名称", &mut form.name, false, FormHint::None, palette);
    form_field(&mut mui, "地址", &mut form.endpoint, false, FormHint::None, palette);
    form_field(&mut mui, "用户名", &mut form.username, false, FormHint::None, palette);
    form_field(
        &mut mui,
        "密码",
        &mut form.password,
        true,
        if is_create {
            FormHint::None
        } else {
            FormHint::KeepSecret
        },
        palette,
    );
    // 证书开关仅 WebDAV（SMB 配置无该字段，勾了也不生效）。
    if form.kind == SourceKind::WebDav {
        form_check(
            &mut mui,
            "忽略证书错误（自签名 HTTPS）",
            &mut form.accept_invalid_certs,
            palette,
        );
    }

    // 测试连接行：按钮 + 状态/结果文本（编辑模式密码留空 = 用已存凭证测）。
    // 注意借用：form 持有 state.edit 字段借用，测试任务只碰 state.source_test
    // 字段（传字段插槽而非整体 &mut state，字段级 disjoint 借用成立）。
    poll_source_test(&mut state.source_test, &ctx);
    let mut test_requested = false;
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 26.0), Sense::hover());
        let tp = mui.painter();
        let running = state.source_test.as_ref().is_some_and(|t| t.running());
        let can_test = !form.name.trim().is_empty() && !form.endpoint.trim().is_empty();
        let gal = tp.layout_no_wrap("测试连接".to_owned(), lib_font(12.0), palette.secondary);
        let w = gal.size().x + 26.0;
        let btn = Rect::from_min_size(pos2(rect.left(), rect.center().y - 15.0), vec2(w, 30.0));
        let enabled = can_test && !running;
        let tr = mui.interact(btn, Id::unique("lib-edit-test"), Sense::click());
        if tr.clicked() && enabled {
            test_requested = true;
        }
        let hover = tr.hovered() && enabled;
        tp.rect_filled(
            btn,
            CornerRadius::same(15),
            if enabled {
                mix(palette.surface, palette.surface_hover, hover as u8 as f32)
            } else {
                fade(palette.surface, 0.5)
            },
        );
        tp.rect_stroke(
            btn.shrink(0.5),
            CornerRadius::same(15),
            Stroke::new(1.0, if enabled { palette.outline } else { fade(palette.outline, 0.5) }),
            StrokeKind::Inside,
        );
        tp.galley(
            btn.min + vec2(13.0, (btn.height() - gal.size().y) / 2.0),
            gal,
            if enabled {
                palette.secondary
            } else {
                fade(palette.secondary, 0.5)
            },
        );
        if tr.hovered() && enabled {
            tr.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        // 状态文本（运行中 spinner 点 / 成功项数 / 失败原因）。
        if let Some(test) = &state.source_test {
            let msg = match &test.result {
                None => Some(("正在连接…".to_owned(), palette.dim)),
                Some(Ok(n)) => Some((format!("连接成功 · {n} 项"), palette.accent)),
                Some(Err(e)) => Some((format!("连接失败: {e}"), palette.danger)),
            };
            if let Some((text, color)) = msg {
                tp.text(
                    pos2(btn.right() + 14.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    lib_font(11.5),
                    color,
                );
            }
        }
    }

    // 按钮行（右对齐）：保存 / 取消。名称或地址为空时保存禁用。
    let form_ok = !form.name.trim().is_empty() && !form.endpoint.trim().is_empty();
    let mut save_requested = false;
    let mut cancel_requested = close_by_mask;
    mui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if accent_pill(ui, "保存", palette, form_ok).clicked() && form_ok {
            save_requested = true;
        }
        if pill_button(ui, "取消", palette, palette.text).clicked() {
            cancel_requested = true;
        }
    });

    if test_requested {
        spawn_source_test(form, &mut state.source_test, &ctx);
    }
    if save_requested || cancel_requested {
        let name = form.name.clone();
        if save_requested {
            state
                .shell_actions
                .push(ShellAction::SaveSource(Box::new(form.clone())));
            state.notify(format!(
                "{}源配置：{name}",
                if is_create { "正在保存新" } else { "正在更新" }
            ));
        }
        state.edit = None;
        state.source_test = None;
    }
}

// ---------------------------------------------------------------------------
// 删除源确认 modal（右键源行触发；确认经 ShellAction::DeleteSource
// 由 main.rs 帧末执行五表级联删除 + keyring 凭证清理）
// ---------------------------------------------------------------------------

fn ui_confirm_delete(parent: &mut Ui, state: &mut BrowseUiState, palette: Palette) {
    let Some((source_id, name)) = state.confirm_delete.clone() else {
        return;
    };
    let screen = parent.max_rect();

    let layer = egui::LayerId::new(egui::Order::Foreground, Id::unique("lib-del-layer"));
    let mut lui = parent.new_child(
        egui::UiBuilder::new()
            .layer_id(layer)
            .max_rect(screen)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    let p = lui.painter();

    // 遮罩：点击空白取消（模态语义，与编辑弹窗一致）。
    p.rect_filled(screen, CornerRadius::ZERO, fade(palette.window, 0.62));
    let mask = lui.interact(screen, Id::unique("lib-del-mask"), Sense::click());
    let mut close = mask.clicked();

    // 居中卡片（窄卡：确认对话框语义）。
    let size = vec2(380.0, 190.0);
    let card = Rect::from_center_size(screen.center(), size);
    p.rect_filled(card, CornerRadius::same(14), palette.panel);
    p.rect_stroke(
        card.shrink(0.5),
        CornerRadius::same(14),
        Stroke::new(1.0, fade(palette.danger, 0.55)),
        StrokeKind::Inside,
    );
    let _card_hit = lui.interact(card, Id::unique("lib-del-card-hit"), Sense::click());

    let mut mui = lui.new_child(
        egui::UiBuilder::new()
            .id_salt("lib-del-card")
            .max_rect(card.shrink2(vec2(24.0, 18.0)))
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    mui.spacing_mut().item_spacing.y = 8.0;

    // 标题。
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 24.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            "删除媒体源",
            lib_font(15.0),
            palette.text,
        );
    }
    // 正文：源名 + 级联说明（危险操作告知范围；文件本身不受影响）。
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 20.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            format!("确定删除「{name}」吗？"),
            lib_font(13.5),
            palette.text,
        );
    }
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 44.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.top() + 10.0),
            Align2::LEFT_TOP,
            "将同时移除：播放历史 · 收藏 · 媒体库条目 · 媒体索引 · 弹幕绑定\n源内文件本身不会被删除",
            lib_font(11.5),
            palette.secondary,
        );
    }

    // 按钮行（右对齐）：危险色删除 / 取消。
    let mut confirmed = false;
    mui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if danger_pill(ui, "删除", palette).clicked() {
            confirmed = true;
        }
        if pill_button(ui, "取消", palette, palette.text).clicked() {
            close = true;
        }
    });

    if confirmed {
        state.shell_actions.push(ShellAction::DeleteSource { source_id, name });
        state.confirm_delete = None;
    } else if close {
        state.confirm_delete = None;
    }
}

/// 播放记录清除确认弹窗（右键单行 / "清除全部"按钮触发）。
fn ui_confirm_clear_history(parent: &mut Ui, state: &mut BrowseUiState, palette: Palette) {
    let Some(target) = state.confirm_clear_history.clone() else {
        return;
    };
    let screen = parent.max_rect();

    let layer = egui::LayerId::new(egui::Order::Foreground, Id::unique("lib-clr-layer"));
    let mut lui = parent.new_child(
        egui::UiBuilder::new()
            .layer_id(layer)
            .max_rect(screen)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    let p = lui.painter();

    p.rect_filled(screen, CornerRadius::ZERO, fade(palette.window, 0.62));
    let mask = lui.interact(screen, Id::unique("lib-clr-mask"), Sense::click());
    let mut close = mask.clicked();

    let size = vec2(380.0, 190.0);
    let card = Rect::from_center_size(screen.center(), size);
    p.rect_filled(card, CornerRadius::same(14), palette.panel);
    p.rect_stroke(
        card.shrink(0.5),
        CornerRadius::same(14),
        Stroke::new(1.0, fade(palette.danger, 0.55)),
        StrokeKind::Inside,
    );
    let _card_hit = lui.interact(card, Id::unique("lib-clr-card-hit"), Sense::click());

    let mut mui = lui.new_child(
        egui::UiBuilder::new()
            .id_salt("lib-clr-card")
            .max_rect(card.shrink2(vec2(24.0, 18.0)))
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    mui.spacing_mut().item_spacing.y = 8.0;

    // 标题 + 确认文案 + 说明行（按 target 类型分派）。
    let (title, confirm_msg, detail) = match &target {
        ClearHistoryTarget::Single { title, .. } => (
            "清除播放记录",
            format!("确定清除「{title}」的播放记录？"),
            "该条记录的进度将被删除，文件本身不受影响".to_string(),
        ),
        ClearHistoryTarget::All => (
            "清除全部播放记录",
            "确定清除全部播放记录？".to_string(),
            "所有播放进度将被删除，文件本身不受影响".to_string(),
        ),
        ClearHistoryTarget::AllSingles { .. } => (
            "清除继续观看记录",
            "确定清除所有继续观看记录？".to_string(),
            "未加入媒体库的单文件播放进度将被删除".to_string(),
        ),
        ClearHistoryTarget::AllLibs { .. } => (
            "清除媒体库播放记录",
            "确定清除所有媒体库播放记录？".to_string(),
            "所有媒体库相关的播放进度将被删除".to_string(),
        ),
        ClearHistoryTarget::Folder { display_name, .. } => (
            "清除播放记录",
            format!("确定清除「{display_name}」的播放记录？"),
            "该媒体库下所有播放进度将被删除".to_string(),
        ),
    };

    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 24.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            title,
            lib_font(15.0),
            palette.text,
        );
    }
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 20.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            confirm_msg,
            lib_font(13.5),
            palette.text,
        );
    }
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 44.0), Sense::hover());
        let mp = mui.painter();
        mp.text(
            pos2(rect.left(), rect.top() + 10.0),
            Align2::LEFT_TOP,
            detail,
            lib_font(11.5),
            palette.secondary,
        );
    }

    let mut confirmed = false;
    mui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if danger_pill(ui, "清除", palette).clicked() {
            confirmed = true;
        }
        if pill_button(ui, "取消", palette, palette.text).clicked() {
            close = true;
        }
    });

    if confirmed {
        state.shell_actions.push(ShellAction::ClearHistory(target));
        state.confirm_clear_history = None;
    } else if close {
        state.confirm_clear_history = None;
    }
}

/// 危险操作按钮：danger 实心胶囊（删除确认专用；与 accent_pill 同工艺）。
fn danger_pill(ui: &mut Ui, label: &str, palette: Palette) -> Response {
    let gal = ui
        .painter()
        .layout_no_wrap(label.to_owned(), lib_font(12.0), palette.on_accent);
    let w = gal.size().x + 30.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let t = ui.ctx().animate_value_with_time(
        Id::unique("lib-danger-pill").with(label),
        resp.hovered() as u8 as f32,
        0.09,
    );
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(15), palette.danger);
    if t > 0.01 {
        // hover 提亮描边（danger 实心底上提亮用白描边，避免再叠 danger）。
        p.rect_stroke(
            rect.shrink(0.5),
            CornerRadius::same(15),
            Stroke::new(1.2, fade(Color32::WHITE, 0.45 * t)),
            StrokeKind::Inside,
        );
    }
    p.galley(
        rect.min + vec2(15.0, (rect.height() - gal.size().y) / 2.0),
        gal,
        palette.on_accent,
    );
    resp
}

/// 后台测试连接（表单值快照；编辑模式密码留空回退 keyring 已存凭证）。
/// 结果经 SourceTest.rx 回流，poll_source_test 每帧收取。
fn spawn_source_test(
    form: &EditSourceForm,
    test_slot: &mut Option<SourceTest>,
    ctx: &egui::Context,
) {
    let source = MediaSource {
        id: form.source_id.unwrap_or(-1),
        name: form.name.trim().to_string(),
        kind: form.kind,
        endpoint: form.endpoint.trim().to_string(),
        username: form.username.trim().to_string(),
        root: PathBuf::new(),
        accept_invalid_certs: form.accept_invalid_certs,
    };
    let password = form.password.clone();
    let use_keyring = form.source_id.is_some() && password.is_empty();
    let (tx, rx) = std::sync::mpsc::channel();
    let repaint = ctx.clone();
    std::thread::Builder::new()
        .name("dyplayer-source-test".into())
        .spawn(move || {
            let opened = if use_keyring {
                source.open_vfs()
            } else {
                source.open_vfs_with_password(&password)
            };
            let result = opened.and_then(|(vfs, root)| {
                vfs.list_dir(&root)
                    .map(|entries| entries.len())
                    .map_err(|e| e.to_string())
            });
            let _ = tx.send(result);
            repaint.request_repaint();
        })
        .ok();
    *test_slot = Some(SourceTest { rx, result: None });
}

/// 每帧收取测试结果（弹框渲染路径内调用）。
fn poll_source_test(test_slot: &mut Option<SourceTest>, ctx: &egui::Context) {
    let mut abandoned = false;
    if let Some(test) = test_slot.as_mut()
        && test.result.is_none()
    {
        match test.rx.try_recv() {
            Ok(r) => test.result = Some(r),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(80));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => abandoned = true,
        }
    }
    if abandoned {
        *test_slot = None;
    }
}

/// 密码输入框提示语（编辑 = 留空保持原凭证；新建 = 无提示）。
#[derive(Clone, Copy, PartialEq)]
enum FormHint {
    None,
    KeepSecret,
}

/// 表单字段：label + 无边框 TextEdit + 聚焦变色的底线。
/// 垂直内边距撑高输入区（TextEdit 单行高度由字体行高决定，无法直接指定）。
fn form_field(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    secret: bool,
    hint: FormHint,
    palette: Palette,
) {
    let (lrect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 17.0), Sense::hover());
    let p = ui.painter();
    let gal = p.layout_no_wrap(label.to_owned(), lib_font(12.5), palette.dim);
    p.galley(
        pos2(lrect.left(), lrect.center().y - gal.size().y / 2.0),
        gal,
        palette.dim,
    );
    let te = egui::TextEdit::singleline(value)
        .id(Id::unique("lib-edit-field").with(label))
        .password(secret)
        .hint_text(match hint {
            FormHint::None => "",
            FormHint::KeepSecret => "留空保持原凭证",
        })
        .font(lib_font(14.5))
        .frame(egui::Frame::new().inner_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 7,
            bottom: 7,
        }))
        .desired_width(ui.available_width());
    let resp = ui.add(te);
    let p = ui.painter();
    let line = Rect::from_min_max(
        pos2(resp.rect.left(), resp.rect.bottom() + 1.0),
        pos2(resp.rect.right(), resp.rect.bottom() + 2.0),
    );
    p.rect_filled(
        line,
        CornerRadius::ZERO,
        if resp.has_focus() {
            palette.accent
        } else {
            palette.outline
        },
    );
}

/// 自绘 checkbox：方框 + Check glyph + 点击切换。
fn form_check(ui: &mut Ui, label: &str, checked: &mut bool, palette: Palette) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
    let p = ui.painter();
    let box_r = Rect::from_center_size(pos2(rect.left() + 9.0, rect.center().y), vec2(17.0, 17.0));
    let stroke_c = if *checked {
        palette.accent
    } else {
        palette.outline
    };
    p.rect_stroke(
        box_r,
        CornerRadius::same(4),
        Stroke::new(1.3, stroke_c),
        StrokeKind::Inside,
    );
    if *checked {
        p.rect_filled(box_r.shrink(2.0), CornerRadius::same(3), palette.accent);
        paint_glyph(p, Glyph::Check, box_r.shrink(4.0), palette.on_accent);
    }
    p.text(
        pos2(box_r.right() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        lib_font(12.5),
        palette.secondary,
    );
    if resp.clicked() {
        *checked = !*checked;
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
}

/// 主操作按钮：accent 实心胶囊（enabled=false 时半透禁用态，点击无效）。
fn accent_pill(ui: &mut Ui, label: &str, palette: Palette, enabled: bool) -> Response {
    let text_c = if enabled {
        palette.on_accent
    } else {
        fade(palette.on_accent, 0.55)
    };
    let gal = ui
        .painter()
        .layout_no_wrap(label.to_owned(), lib_font(12.0), text_c);
    let w = gal.size().x + 30.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let t = if enabled {
        ui.ctx().animate_value_with_time(
            Id::unique("lib-acc-pill").with(label),
            resp.hovered() as u8 as f32,
            0.09,
        )
    } else {
        0.0
    };
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(15),
        if enabled {
            mix(palette.accent, palette.accent_hover, t)
        } else {
            fade(palette.accent, 0.45)
        },
    );
    p.galley(
        rect.min + vec2(15.0, (rect.height() - gal.size().y) / 2.0),
        gal,
        text_c,
    );
    resp
}

// ---------------------------------------------------------------------------
// 图标目录浏览器（开发工具：modernz-icons.ttf 未使用 PUA 图标）
// ---------------------------------------------------------------------------

/// 浏览器窗口：分组标题 + 自适应列数网格；点击图标复制 Rust 字面量。
/// 图标字符经全局字体回退链命中 modernz-icons（PUA 码点专属）。
fn icon_browser_window(parent: &mut Ui, state: &mut BrowseUiState, palette: Palette) {
    if !state.show_icon_browser {
        return;
    }
    let mut open = state.show_icon_browser;
    egui::Window::new("图标目录 · 未使用图标")
        .id(Id::unique("lib-icon-browser"))
        .open(&mut open)
        .default_size([640.0, 460.0])
        .min_width(320.0)
        .collapsible(false)
        .show(parent.ctx(), |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "共 {} 个未使用图标 · 点击复制 Rust 字面量",
                        crate::icon_catalog::all().count()
                    ))
                    .family(lib_family())
                    .small()
                    .weak(),
                );
                if let Some((cp, name)) = state.copied_icon {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "已复制 {name}  '\\u{{{:04X}}}'",
                                cp as u32
                            ))
                            .family(lib_family())
                            .small()
                            .color(palette.accent),
                        );
                    });
                }
            });
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    icon_group(ui, state, palette, "Material", crate::icon_catalog::MATERIAL);
                    icon_group(ui, state, palette, "Fluent", crate::icon_catalog::FLUENT);
                    icon_group(ui, state, palette, "其它", crate::icon_catalog::OTHER);
                });
        });
    state.show_icon_browser = open;
}

/// 单个分组：标题 + 网格行。列数按可用宽度自适应（cell 140px + 间距 6px）。
fn icon_group(
    ui: &mut Ui,
    state: &mut BrowseUiState,
    palette: Palette,
    title: &str,
    icons: &[(char, &'static str)],
) {
    let hover_bg = fade(palette.accent, 0.18);
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(format!("{title}（{} 个）", icons.len()))
            .family(lib_family())
            .strong()
            .small(),
    );
    ui.add_space(3.0);
    let cell_w = 140.0;
    let cell_h = 36.0;
    let step = cell_w + 6.0;
    let cols = (((ui.available_width() - 2.0) / step).floor() as usize).clamp(1, 8);
    for row in icons.chunks(cols) {
        ui.horizontal(|ui| {
            for &(cp, name) in row {
                let (rect, resp) = ui.allocate_exact_size(vec2(cell_w, cell_h), Sense::click());
                let p = ui.painter();
                let hovered = resp.hovered();
                if hovered {
                    p.rect_filled(rect, CornerRadius::same(4), hover_bg);
                }
                // hover 时图标放大（19 → 24），锚点左移避免压到名字文本。
                let (icon_x, icon_size) = if hovered {
                    (rect.left() + 1.0, 24.0)
                } else {
                    (rect.left() + 3.0, 19.0)
                };
                p.text(
                    pos2(icon_x, rect.center().y),
                    Align2::LEFT_CENTER,
                    cp,
                    lib_font(icon_size),
                    if hovered { palette.accent } else { palette.text },
                );
                p.text(
                    pos2(rect.left() + 27.0, rect.top() + 12.0),
                    Align2::LEFT_CENTER,
                    icon_shorten_name(name),
                    lib_font(10.0),
                    palette.dim,
                );
                p.text(
                    pos2(rect.left() + 27.0, rect.bottom() - 10.0),
                    Align2::LEFT_CENTER,
                    format!("\\u{{{:04X}}}", cp as u32),
                    lib_font(9.0),
                    fade(palette.dim, 0.6),
                );
                if resp.clicked() {
                    ui.ctx().copy_text(format!("'\\u{{{:04X}}}'", cp as u32));
                    state.copied_icon = Some((cp, name));
                }
                resp.on_hover_text(format!("{name}\n'\\u{{{:04X}}}'（点击复制）", cp as u32));
            }
        });
    }
}

/// 网格 cell 内的名字超长截断（全名见 hover tooltip）。
fn icon_shorten_name(name: &str) -> String {
    const MAX: usize = 19;
    if name.chars().count() > MAX {
        let cut: String = name.chars().take(MAX - 1).collect();
        format!("{cut}…")
    } else {
        name.to_owned()
    }
}

/// 标题栏风格映射（主题切换 / 明暗切换时标题栏跟随变色）。
pub fn titlebar_style(state: &BrowseUiState, dark: bool) -> crate::titlebar::TitleBarStyle {
    let p = palettes(dark)[state.theme_idx];
    crate::titlebar::TitleBarStyle {
        bg: p.window,
        text: p.text,
        dim: p.dim,
        hover: p.surface_hover,
        danger: p.danger,
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑工具（可测试）
// ---------------------------------------------------------------------------


fn fmt_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "00:00".into();
    }
    let total = t as u64;
    let (h, m, s) = (total / 3600, total % 3600 / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 观看进度 [0,1]：dur 无效 → 0，越界 clamp。
fn progress_frac(pos: f64, dur: f64) -> f32 {
    if !pos.is_finite() || !dur.is_finite() || dur <= 0.0 || pos <= 0.0 {
        return 0.0;
    }
    ((pos / dur) as f32).clamp(0.0, 1.0)
}

/// "2026-10-19 12:34:56" → "10-19 12:34"；异常长度原样返回（截 16 字符）。
fn short_stamp(s: &str) -> String {
    if s.len() >= 19 && s.as_bytes().get(4) == Some(&b'-') {
        s[5..16].to_string()
    } else {
        s.chars().take(16).collect()
    }
}

/// 源徽章文本（跨源区分；source_id=0 为拖拽本机文件，源删除时明示）。
fn source_label(sources: &[MediaSource], source_id: i64) -> String {
    match sources.iter().find(|s| s.id == source_id) {
        Some(s) => format!("{} · {}", s.kind.label(), s.name),
        None if source_id == 0 => "本机".into(),
        None => "源已删除".into(),
    }
}

/// 行右侧元信息两行锚点（第一行源徽章 / 第二行播放时间）：
/// 同 x 左对齐成列，与左侧标题/副行共享行网格（top+17 / top+35，行距 18px）。
fn meta_anchors(rect: egui::Rect, title_max_w: f32) -> [egui::Pos2; 2] {
    let x = rect.left() + 50.0 + title_max_w;
    [pos2(x, rect.top() + 17.0), pos2(x, rect.top() + 35.0)]
}

/// 宽度 → 网格列数（至少 1 列）。
fn columns_for_width(w: f32) -> usize {
    if w <= CARD_W {
        return 1;
    }
    ((w + CARD_GAP) / (CARD_W + CARD_GAP)).floor().max(1.0) as usize
}


// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_frac_edges() {
        assert_eq!(progress_frac(0.0, 100.0), 0.0);
        assert_eq!(progress_frac(-5.0, 100.0), 0.0);
        assert_eq!(progress_frac(50.0, 0.0), 0.0);
        assert_eq!(progress_frac(50.0, -1.0), 0.0);
        assert_eq!(progress_frac(f64::NAN, 100.0), 0.0);
        assert!((progress_frac(30.0, 120.0) - 0.25).abs() < 1e-6);
        assert_eq!(progress_frac(200.0, 100.0), 1.0);
    }

    #[test]
    fn lib_sub_label_episode_semantics() {
        // 多集：在看 / 全看完才是已看 / 部分看完 / 未观看。
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 12,
                watching: 3,
                finished: 2,
                watching_frac_sum: 1.5
            }),
            "3 集在看"
        );
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 12,
                watching: 0,
                finished: 12,
                watching_frac_sum: 0.0
            }),
            "已看"
        );
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 12,
                watching: 0,
                finished: 5,
                watching_frac_sum: 0.0
            }),
            "已看 5/12 集"
        );
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 12,
                watching: 0,
                finished: 0,
                watching_frac_sum: 0.0
            }),
            "未观看"
        );
        // 单视频条目：在看 / 已看 / 单集视频。
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 1,
                watching: 1,
                finished: 0,
                watching_frac_sum: 0.4
            }),
            "在看"
        );
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 1,
                watching: 0,
                finished: 1,
                watching_frac_sum: 0.0
            }),
            "已看"
        );
        assert_eq!(lib_sub_label(&EpisodeStats::default()), "未观看");
        assert_eq!(
            lib_sub_label(&EpisodeStats {
                total: 1,
                ..Default::default()
            }),
            "单集视频"
        );
    }

    #[test]
    fn lib_progress_mixed_finished_and_watching() {
        // (2 看完 + 在看进度和 1.5) / 12 集。
        let stats = EpisodeStats {
            total: 12,
            watching: 3,
            finished: 2,
            watching_frac_sum: 1.5,
        };
        assert!((lib_progress(&stats) - (2.0 + 1.5) / 12.0).abs() < 1e-6);
        // 全看完 = 1.0；空库 = 0。
        assert_eq!(lib_progress(&EpisodeStats {
            total: 3,
            finished: 3,
            ..Default::default()
        }), 1.0);
        assert_eq!(lib_progress(&EpisodeStats::default()), 0.0);
    }

    #[test]
    fn detect_cover_matches_conventions() {
        let mk = |name: &str, is_dir: bool, path: &str| dyplayer_vfs::VfsEntry {
            name: name.into(),
            path: std::path::PathBuf::from(path),
            is_dir,
            size: 0,
            modified: None,
        };
        let entries = vec![
            mk("EP01.mkv", false, "d/EP01.mkv"),
            mk("Subs", true, "d/Subs"),
            mk("Cover.JPG", false, "d/Cover.JPG"), // 大小写不敏感
        ];
        assert_eq!(detect_cover(&entries).as_deref(), Some("d/Cover.JPG"));
        let none = vec![mk("EP01.mkv", false, "d/EP01.mkv")];
        assert!(detect_cover(&none).is_none());
    }



    #[test]
    fn short_stamp_formats_sqlite_timestamp() {
        assert_eq!(short_stamp("2026-10-19 12:34:56"), "10-19 12:34");
        assert_eq!(short_stamp("short"), "short");
        assert_eq!(short_stamp(""), "");
    }

    #[test]
    fn meta_anchors_two_rows_left_aligned_and_spaced() {
        let rect = egui::Rect::from_min_size(pos2(12.0, 40.0), vec2(600.0, 56.0));
        let title_max_w = rect.width() - 46.0 - 150.0;
        let [top, sub] = meta_anchors(rect, title_max_w);
        assert_eq!(top.x, sub.x);
        assert!((top.x - (rect.left() + 50.0 + title_max_w)).abs() < 1e-6);
        assert!((top.y - (rect.top() + 17.0)).abs() < 1e-6);
        assert!((sub.y - (rect.top() + 35.0)).abs() < 1e-6);
        assert!((sub.y - top.y - 18.0).abs() < 1e-6);
        assert!(sub.x < rect.right() - 60.0);
    }

    #[test]
    fn columns_for_width_matches_grid() {
        assert_eq!(columns_for_width(100.0), 1);
        assert_eq!(columns_for_width(CARD_W), 1);
        assert_eq!(columns_for_width(CARD_W + CARD_GAP - 1.0), 1);
        assert_eq!(columns_for_width(CARD_W + CARD_GAP), 1);
        assert_eq!(columns_for_width(CARD_W * 2.0 + CARD_GAP), 2);
        assert_eq!(columns_for_width(CARD_W * 5.0 + CARD_GAP * 4.0 + 1.0), 5);
    }

    #[test]
    fn edit_form_prefills_source_fields() {
        let source = MediaSource {
            id: 7,
            kind: SourceKind::WebDav,
            name: "nas".into(),
            endpoint: "https://nas.local/dav".into(),
            username: "u".into(),
            root: std::path::PathBuf::from("/"),
            accept_invalid_certs: true,
        };
        let form = EditSourceForm::from_source(&source);
        assert_eq!(form.source_id, Some(7));
        assert_eq!(form.kind, SourceKind::WebDav);
        assert_eq!(form.name, "nas");
        assert_eq!(form.endpoint, "https://nas.local/dav");
        assert_eq!(form.username, "u");
        // 密码不回显（留空 = 保持原凭证，语义同正式编辑对话框）。
        assert!(form.password.is_empty());
        assert!(form.accept_invalid_certs);
    }

    #[test]
    fn new_source_form_is_create_mode() {
        let form = EditSourceForm::new_source(SourceKind::Smb);
        assert_eq!(form.source_id, None);
        assert_eq!(form.kind, SourceKind::Smb);
        assert!(form.name.is_empty());
        assert!(form.endpoint.is_empty());
        assert!(form.username.is_empty());
        assert!(form.password.is_empty());
        assert!(!form.accept_invalid_certs);
    }

    fn lib_entry(id: i64, source_id: i64, folder: &str) -> MediaLibEntry {
        MediaLibEntry {
            id,
            source_id,
            folder_path: folder.into(),
            display_name: folder.into(),
            cover_path: None,
            cover_probed: true,
            pinned: false,
            added_at: String::new(),
        }
    }

    fn rec_entry(source_id: i64, path: &str, finished: bool, updated: &str) -> RecentEntry {
        RecentEntry {
            source_id,
            path: path.into(),
            title: path.into(),
            position_sec: 30.0,
            duration_sec: 100.0,
            finished,
            http_headers: None,
            updated_at: updated.into(),
        }
    }

    #[test]
    fn split_continuing_partitions_by_media_lib() {
        let lib = vec![
            lib_entry(1, 5, "D:/shows/A"),
            lib_entry(2, 5, "D:/shows/AB"),
            lib_entry(3, 9, "D:/movies/m.mkv"),
        ];
        let recent = vec![
            rec_entry(5, "D:/shows/A/ep1.mkv", false, "2026-10-01 10:00:00"),
            rec_entry(5, "D:/shows/A/ep2.mkv", false, "2026-10-03 10:00:00"),
            // AB 边界：不误配 A 条目；已看完 → 整体排除。
            rec_entry(5, "D:/shows/AB/x.mkv", true, "2026-10-05 10:00:00"),
            // 不属于任何媒体库 → 单文件。
            rec_entry(5, "D:/stray/b.mkv", false, "2026-10-02 10:00:00"),
            // 单视频条目精确匹配 → 媒体库视角。
            rec_entry(9, "D:/movies/m.mkv", false, "2026-10-04 10:00:00"),
            // 同路径但 source_id 不同 → 不归该条目（单文件）。
            rec_entry(7, "D:/shows/A/other.mkv", false, "2026-10-06 10:00:00"),
        ];
        let (singles, libs) = split_continuing(&recent, &lib);
        let single_paths: Vec<&str> = singles.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(single_paths, vec!["D:/stray/b.mkv", "D:/shows/A/other.mkv"]);
        // 聚合按组内最新访问降序：A（10-03）→ 单视频（10-04）？
        // A 组最新 10-03，单视频 10-04 更新 → 单视频在前。
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].id, 3);
        assert_eq!(libs[1].id, 1);
    }

    #[test]
    fn split_continuing_both_empty_without_history() {
        let lib = vec![lib_entry(1, 5, "D:/shows/A")];
        let (singles, libs) = split_continuing(&[], &lib);
        assert!(singles.is_empty());
        assert!(libs.is_empty());
        // 看完的记录不进任何一侧（非"进行中"）。
        let finished = vec![rec_entry(5, "D:/shows/A/ep1.mkv", true, "2026-10-01 10:00:00")];
        let (singles, libs) = split_continuing(&finished, &lib);
        assert!(singles.is_empty());
        assert!(libs.is_empty());
    }

    #[test]
    fn split_continuing_windows_separator_and_prefix_boundary() {
        let lib = vec![lib_entry(1, 5, "D:\\shows\\A")];
        let recent = vec![
            // 反斜杠归一后命中 A。
            rec_entry(5, "D:\\shows\\A\\ep1.mkv", false, "2026-10-01 10:00:00"),
            // AB 边界：不命中。
            rec_entry(5, "D:\\shows\\AB\\x.mkv", false, "2026-10-02 10:00:00"),
        ];
        let (singles, libs) = split_continuing(&recent, &lib);
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].id, 1);
        assert_eq!(singles.len(), 1);
        assert_eq!(singles[0].path, "D:\\shows\\AB\\x.mkv");
    }

    #[test]
    fn palettes_have_unique_names_and_visible_accent() {
        for group in [crate::theme::palettes(true), crate::theme::palettes(false)] {
            let names: Vec<_> = group.iter().map(|p| p.name).collect();
            for (i, n) in names.iter().enumerate() {
                assert!(!n.is_empty());
                for (j, m) in names.iter().enumerate() {
                    if i != j {
                        assert_ne!(n, m, "duplicate palette name");
                    }
                }
            }
            for p in group.iter() {
                // accent 与 panel 需有可感知对比（主题层的最低要求）。
                let dr = (p.accent.r() as i32 - p.panel.r() as i32).abs();
                let dg = (p.accent.g() as i32 - p.panel.g() as i32).abs();
                let db = (p.accent.b() as i32 - p.panel.b() as i32).abs();
                assert!(
                    dr + dg + db > 120,
                    "accent too close to panel in {}",
                    p.name
                );
            }
        }
    }

    #[test]
    fn light_palettes_paired_and_readable() {
        let (dark_group, light_group) =
            (crate::theme::palettes(true), crate::theme::palettes(false));
        // 深浅两组等长配对（theme_idx 共用，切明暗不越界）。
        assert_eq!(dark_group.len(), light_group.len());
        // palettes() 按明暗选组。
        assert!(std::ptr::eq(
            palettes(true).as_ptr(),
            dark_group.as_ptr()
        ));
        assert!(std::ptr::eq(
            palettes(false).as_ptr(),
            light_group.as_ptr()
        ));
        for p in light_group.iter() {
            // 浅色板：正文文本须显著深于卡面（可读性下限）。
            let dr = (p.text.r() as i32 - p.panel.r() as i32).abs();
            let dg = (p.text.g() as i32 - p.panel.g() as i32).abs();
            let db = (p.text.b() as i32 - p.panel.b() as i32).abs();
            assert!(dr + dg + db > 400, "text too close to panel in {}", p.name);
            // accent 上的文字（on_accent 白）须与 accent 有对比。
            let ar = (p.on_accent.r() as i32 - p.accent.r() as i32).abs();
            let ag = (p.on_accent.g() as i32 - p.accent.g() as i32).abs();
            let ab = (p.on_accent.b() as i32 - p.accent.b() as i32).abs();
            assert!(
                ar + ag + ab > 300,
                "on_accent too close to accent in {}",
                p.name
            );
        }
    }

    #[test]
    fn mix_and_fade_gamma_space() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(100, 100, 100);
        let m = mix(a, b, 0.5);
        assert_eq!(m.r(), 50);
        let f = fade(Color32::from_rgb(200, 100, 50), 1.0);
        assert_eq!((f.r(), f.g(), f.b(), f.a()), (200, 100, 50, 255));
    }
}
