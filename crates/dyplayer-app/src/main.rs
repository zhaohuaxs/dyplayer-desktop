// release：GUI 子系统（双击 exe 不弹控制台）；debug：保留控制台看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod browse_ui;
mod danmaku_ui;
mod drop_play;
mod fs_patch;
mod glyph;
#[cfg(not(feature = "backend-wgpu"))]
mod gl_loader;

mod icon_catalog;
mod osc_state;
#[cfg(feature = "backend-wgpu")]
mod overlay_window;
#[cfg(not(feature = "backend-wgpu"))]
#[path = "overlay_window_glow.rs"]
mod overlay_window;

#[cfg_attr(not(feature = "backend-wgpu"), allow(dead_code))]
mod sw_render_thread;
mod player_app;
mod refresh;
mod subtitles;
mod theme;
mod thumb_gen;
mod titlebar;
#[cfg(feature = "backend-wgpu")]
mod wid_host;
#[cfg(not(feature = "backend-wgpu"))]
#[path = "wid_host_glow.rs"]
mod wid_host;

use std::path::PathBuf;
use std::sync::Arc;

use browser::{BrowserState, BrowserView, MediaSource, SourceKind};

use player_app::{FreezeProbe, PH_IDLE, PendingPlay, PlayerSession, phase_name};

enum Screen {
    Browse,
    Player,

}

struct DyPlayerApp {
    screen: Screen,

    egui_ctx: egui::Context,
    sources: Vec<MediaSource>,
    browser: BrowserState,
    player: Option<PlayerSession>,
    config: dyplayer_store::AppConfig,
    store: Arc<dyplayer_store::Store>,
    libmpv_path: Option<PathBuf>,
    notice: Option<String>,
    /// 媒体库集数统计后台回流通道（单批单线程，逐条发送）。
    lib_stats_rx: Option<std::sync::mpsc::Receiver<(i64, browse_ui::EpisodeStats)>>,
    /// 封面纹理后台回流通道（VFS read_full + image 解码 → ColorImage；
    /// UI 线程 ctx.load_texture 转 TextureHandle 存 cover_cache）。
    cover_load_rx: Option<std::sync::mpsc::Receiver<(String, Option<egui::ColorImage>)>>,
    /// 抽帧缩略图后台回流通道（headless mpv 抽帧 → ColorImage；
    /// UI 线程 ctx.load_texture 转 TextureHandle 存 thumb_cache）。
    thumb_gen_rx: Option<std::sync::mpsc::Receiver<thumb_gen::ThumbResult>>,
    /// 拖放组队后台回流通道（canonicalize + 目录扫描在后台线程，
    /// UI 线程 poll 后执行 start_play / XML 弹幕应用 / 通知）。
    pending_drop: Option<std::sync::mpsc::Receiver<drop_play::DropPlayPlan>>,
    /// 启动时刻（媒体索引每日刷新的延迟基准）。
    startup_at: std::time::Instant,
    /// 今天还没刷新过媒体索引（app_meta.last_index_date != 本地今天）。
    index_refresh_pending: bool,
    /// 本会话已派发刷新线程（防重复 spawn）。
    index_refresh_started: bool,
    /// 隐藏创建的窗口是否已显示（logic() 首帧一次性：装样式 + ShowWindow）。
    viewport_shown: bool,

    browse_ui: browse_ui::BrowseUiState,
    /// 全局明暗主题（跟随系统/浅色/深色；播放页设置窗改动经会话镜像回流）。
    theme_mode: dyplayer_store::ThemeMode,
    /// 已应用的全局样式快照（明暗 + 主题色索引）：仅变化时重映射 egui
    /// Style（set_theme 会重置 style，切换时置 None 强制重应用）。
    style_applied: Option<(bool, u8)>,

    /// 冻结取证探针（watchdog 线程共享）。
    probe: Arc<FreezeProbe>,
    /// 串流播放弹窗。
    stream_dialog_open: bool,
    stream_url: String,
    stream_headers: String,
    /// glow GL context（glow 后端 only；wgpu 后端不存在此字段）。
    #[cfg(not(feature = "backend-wgpu"))]
    gl: Option<std::sync::Arc<eframe::glow::Context>>,
}

impl DyPlayerApp {
    fn new(cc: &eframe::CreationContext) -> anyhow::Result<Self> {
        let mut config = dyplayer_store::AppConfig::load_or_default()?;
        // GPU 渲染路径门禁：仅纯软件 WARP（所有适配器均为 Cpu）时禁用——
        // vo=gpu-next 的 D3D11 呈现在无任何 GPU 驱动的环境不可用 → 视频区黑屏。
        // SVGA3D/远程桌面虚拟 GPU（Virtual/Other）支持 D3D11，放行让 mpv 自试；
        // GPU 路径黑屏的真根因是 libmpv 裁剪版（移除 vo=gpu-next）而非适配器类型。
        // 有非 Cpu 适配器即放行；配置已存 gpu 时仅在纯 WARP 下静默回退 sw
        // （仅内存不落盘——换回有 GPU 的机器偏好仍生效）。
        #[cfg(feature = "backend-wgpu")]
        let hw_gpu = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| {
                rs.available_adapters
                    .iter()
                    .any(|a| a.get_info().device_type != wgpu::DeviceType::Cpu)
            })
            .unwrap_or(true);
        #[cfg(not(feature = "backend-wgpu"))]
        let hw_gpu = false;
        if !hw_gpu && config.engine.render_path.eq_ignore_ascii_case("gpu") {
            tracing::warn!(
                "GPU 渲染路径不可用（未检测到硬件 GPU 适配器），本次启动回退 SW 渲染（配置保持不落盘）"
            );
            config.engine.render_path = "sw".into();
        }
        // 启动默认视图（设置窗"启动时进入播放页"）。
        let startup_screen = match config.ui.startup_view {
            dyplayer_store::StartupView::Player => Screen::Player,
            dyplayer_store::StartupView::Library => Screen::Browse,
        };
        // 启动即定主题（否则窗口先以 egui 默认 visuals 运行），并按首帧
        // 底色预涂客户区：建窗→首帧 present 之间是 Win32 白底，初始化耗时
        // 期间会闪出浅色（new() 的 SQLite/字体解析）。播放页无论明暗主题
        // 首帧都是黑底（空态黑），预涂黑；其他页预涂主题 panel_fill。
        let dark = theme::effective_dark(config.ui.theme);
        cc.egui_ctx.set_theme(if dark {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
        // 启动即映射语义色（panel_fill 等），窗口预涂色与首帧观感一致。
        theme::apply_style(&cc.egui_ctx, dark, config.ui.accent as usize);
        let prepaint = if matches!(startup_screen, Screen::Player) {
            egui::Color32::BLACK
        } else {
            cc.egui_ctx.style_of(cc.egui_ctx.theme()).visuals.panel_fill
        };
        theme::prepaint_window("DyPlayer", prepaint);
        // 隐藏创建保底唤醒：窗口隐藏期间 eframe 只跑 logic 不跑 ui pass，
        // 必须有 repaint 请求 logic 才会被调（否则窗口永不显示）。
        cc.egui_ctx.request_repaint();
        // 图片加载器（file:// 等 URI）：媒体库封面等本地图片直显。
        // SVG 图标（house 等）：App 启动装主 ctx——GPU 路径下主线程不跑
        // osc::draw，若只依赖播放页首帧安装，媒体库页/空态页的 SVG 全灭。
        egui_extras::install_image_loaders(&cc.egui_ctx);
        player_app::osc::install_svg_icons(&cc.egui_ctx);
        player_app::install_cjk_font(&cc.egui_ctx, &config.ui.font);
        let store = Arc::new(dyplayer_store::Store::open_default()?);
        let libmpv_path = std::env::var("DYPLAYER_LIBMPV")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                let configured = config.engine.libmpv_path.clone();
                if configured.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(configured))
                }
            });

        let theme_mode = config.ui.theme;
        // 每天首次打开应用 → 后台延迟重建媒体索引（当天标记落 app_meta）。
        // 先于 Self 构造求值（store 随构造 move 进字段）。
        let index_refresh_pending = store
            .get_meta("last_index_date")
            .ok()
            .flatten()
            .as_deref()
            != Some(store.local_date().as_str());
        let mut app = Self {
            screen: startup_screen,

            egui_ctx: cc.egui_ctx.clone(),
            sources: Vec::new(),
            browser: BrowserState::default(),
            player: None,
            config,
            store,
            libmpv_path,
            notice: None,
            lib_stats_rx: None,
            cover_load_rx: None,
            thumb_gen_rx: None,
            pending_drop: None,
            startup_at: std::time::Instant::now(),
            index_refresh_pending,
            index_refresh_started: false,

            browse_ui: browse_ui::BrowseUiState::new(),
            theme_mode,
            style_applied: None,

            probe: Arc::new(FreezeProbe::default()),
            viewport_shown: false,
            stream_dialog_open: false,
            stream_url: String::new(),
            stream_headers: String::new(),
            #[cfg(not(feature = "backend-wgpu"))]
            gl: cc.gl.clone(),
        };
        spawn_freeze_watchdog(Arc::clone(&app.probe));
        app.browse_ui.hw_gpu_available = hw_gpu;
        app.reload_sources();
        app.browse_ui.sync_ui_cfg(&app.config.ui, &app.config.engine);
        app.browse_ui.ensure_data(&app.store, &mut app.browser);

        if !app.sources.is_empty() {
            app.browser.set_active_source(Some(0), &app.sources);
            app.request_active_root();
        }
        Ok(app)
    }

    fn reload_sources(&mut self) {
        self.sources = match self.load_sources() {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!("load sources failed: {e}");
                Vec::new()
            }
        };
        if self
            .browser
            .active_source
            .is_some_and(|i| i >= self.sources.len())
        {
            self.browser.set_active_source(None, &self.sources);
        }
    }

    fn load_sources(&self) -> anyhow::Result<Vec<MediaSource>> {
        self.store.with_connection(query_sources)
    }


    fn add_local_source(&mut self, root: PathBuf) {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        if let Err(e) = dyplayer_vfs::LocalFs::new(&root) {
            self.notice = Some(format!("无法添加源 {}: {e}", root.display()));
            return;
        }
        let endpoint = root.display().to_string();
        if let Err(e) = self.store.add_source("local", &name, &endpoint, "", "{}") {
            self.notice = Some(format!("保存源失败: {e}"));
            return;
        }
        self.reload_sources();
        self.browser
            .set_active_source(self.sources.len().checked_sub(1), &self.sources);
        self.browser.view = BrowserView::Files;
        self.browser.refresh_favorites(&self.store);
        self.request_active_root();
    }


    fn add_remote_source(&mut self, form: browse_ui::EditSourceForm) {
        let extra_json =
            serde_json::json!({ "accept_invalid_certs": form.accept_invalid_certs }).to_string();
        let source_id = match self.store.add_source(
            form.kind.as_db(),
            &form.name,
            &form.endpoint,
            &form.username,
            &extra_json,
        ) {
            Ok(id) => id,
            Err(e) => {
                self.notice = Some(format!("保存源失败: {e}"));
                return;
            }
        };
        if !form.password.is_empty()
            && let Err(e) = dyplayer_store::secrets::set_source_credential(source_id, &form.password)
        {
            tracing::warn!("保存凭证失败（源已入库）: {e}");
            self.notice = Some(format!("源已保存，但凭证写入失败: {e}"));
        }
        self.reload_sources();
        self.browser
            .set_active_source(self.sources.len().checked_sub(1), &self.sources);
        self.browser.view = BrowserView::Files;
        self.browser.refresh_favorites(&self.store);
        self.request_active_root();
    }

    fn update_remote_source(&mut self, source_id: i64, form: browse_ui::EditSourceForm) {
        let extra_json =
            serde_json::json!({ "accept_invalid_certs": form.accept_invalid_certs }).to_string();
        if let Err(e) = self.store.update_source(
            source_id,
            form.kind.as_db(),
            &form.name,
            &form.endpoint,
            &form.username,
            &extra_json,
        ) {
            self.notice = Some(format!("更新源失败: {e}"));
            return;
        }
        // 密码留空 = 保持原凭证；仅在用户填写了新密码时覆盖 keyring。
        if !form.password.is_empty()
            && let Err(e) = dyplayer_store::secrets::set_source_credential(source_id, &form.password)
        {
            tracing::warn!("保存凭证失败: {e}");
            self.notice = Some(format!("源已更新，但凭证写入失败: {e}"));
        }
        self.reload_sources();
        // 编辑的是当前活跃源时刷新目录（地址/凭据可能已变）。
        if let Some(idx) = self.browser.active_source
            && self.sources.get(idx).is_some_and(|s| s.id == source_id)
        {
            self.browser.view = BrowserView::Files;
            self.request_active_root();
        }
    }

    /// 删除媒体源：store 五表级联（sources/play_history/favorites/
    /// media_library/media_index/danmaku_bindings）+ keyring 凭证清理。
    /// 删的是活跃源时重置浏览状态：无源 → 空列表；有剩余 → 切首源。
    fn delete_source(&mut self, source_id: i64, name: String) {
        if let Err(e) = self.store.remove_source(source_id) {
            self.browse_ui.notify(format!("删除源失败: {e}"));
            return;
        }
        // keyring 凭证尽力清理（失败不阻塞删除，源行已不存在）。
        if let Err(e) = dyplayer_store::secrets::delete_source_credential(source_id) {
            tracing::warn!("删除源凭证失败（源已删除）: {e}");
        }
        // 活跃源被删：清浏览缓存，重置到首源（或空态）。
        let was_active = self
            .browser
            .active_source_id
            .is_some_and(|id| id == source_id);
        self.reload_sources();
        if was_active {
            self.browser.entries.clear();
            self.browser.dir = PathBuf::new();
            self.browse_ui.source_root = None;
            self.browse_ui.clear_index_search();
            if self.sources.is_empty() {
                self.browser.set_active_source(None, &self.sources);
                self.browse_ui.selected_source = None;
            } else {
                self.browser.set_active_source(Some(0), &self.sources);
                self.browse_ui.selected_source = Some(0);
                self.browser.view = BrowserView::Files;
                self.request_active_root();
            }
        } else {
            // 侧栏选中态索引可能位移（删除导致重排）。
            self.browse_ui.selected_source = self.browser.active_source;
        }
        // 媒体库/统计/收藏缓存全量重拉（ensure_data 的 stamp 变化会清
        // lib_stats，这里显式同步保证下帧即新数据）。
        self.browse_ui.ensure_data(&self.store, &mut self.browser);
        self.browse_ui.notify(format!("已删除媒体源：{name}"));
    }

    /// 异步打开当前选中源的浏览根（需要 ctx 发起后台请求）。
    fn request_active_root(&mut self) {
        let Some(idx) = self.browser.active_source else {
            return;
        };
        let Some(source) = self.sources.get(idx) else {
            return;
        };
        let (vfs, root) = match source.open_vfs() {
            Ok(pair) => pair,
            Err(e) => {
                self.browser.error = Some(e);
                return;
            }
        };
        drop(vfs);
        // 记录浏览根（shell 面包屑相对段拆解用；各源根语义不同无法从
        // MediaSource 直接推导——本地是 canonicalize 路径，SMB 是 VFS 根）。
        self.browse_ui.source_root = Some(root.clone());
        let source = source.clone();
        let ctx = self.egui_ctx.clone();
        self.browser.request_dir(&source, root, &ctx);
    }

    /// Shell 开播请求：定位源并异步组队（完成经 poll_dir 回流 start_play）。
    fn request_folder_play(
        &mut self,
        source_id: i64,
        dir: PathBuf,
        clicked: Option<PathBuf>,
        ctx: &egui::Context,
    ) {
        let Some(source) = self.sources.iter().find(|s| s.id == source_id).cloned() else {
            self.notice = Some("该条目所属的源已不存在".into());
            return;
        };
        self.browser.request_folder_play(&source, dir, clicked, ctx);
    }

    /// Shell 目录导航：当前激活源内进入子目录（浏览视图文件夹卡）。
    fn request_browse_into(&mut self, dir: PathBuf) {
        let Some(idx) = self.browser.active_source else {
            return;
        };
        let Some(source) = self.sources.get(idx).cloned() else {
            return;
        };
        let ctx = self.egui_ctx.clone();
        self.browser.request_dir(&source, dir, &ctx);
    }

    fn start_play(&mut self, ctx: &egui::Context, pending: PendingPlay) {
        if let Some(player) = &mut self.player {
            // 复用会话路径（媒体库点视频重进）：恢复 GPU 宿主/覆盖层——
            // 离开时 set_player_screen_active(false) 的收起状态若不在此
            // 恢复，覆盖层保持隐藏（visible 残留 false）、宿主停留屏幕外。
            player.set_player_screen_active(true);
            player.load(pending);
        } else {
            if let Err(e) = self.create_player_session(ctx, pending) {
                self.notice = Some(format!("启动播放引擎失败: {e}"));
                return;
            }
        }
        self.screen = Screen::Player;
    }

    /// 按当前 config 创建播放会话（无旧会话路径）：开播（start_play）与
    /// 渲染路径重建共用。调用方负责切屏与续位（set_pending_resume）。
    fn create_player_session(
        &mut self,
        ctx: &egui::Context,
        pending: PendingPlay,
    ) -> Result<(), String> {
        match PlayerSession::new(
            ctx.clone(),
            pending,
            self.config.player.clone(),
            self.config.danmaku.clone(),
            &self.config.engine,
            Arc::clone(&self.store),
            self.libmpv_path.as_deref(),
            #[cfg(not(feature = "backend-wgpu"))]
            self.gl.as_ref().expect("glow context missing").clone(),
        ) {
            Ok(mut session) => {
                session.probe = Some(Arc::clone(&self.probe));
                session.set_ui_startup_view(self.config.ui.startup_view);
                session.set_ui_theme(self.config.ui.theme);
                self.player = Some(session);
                Ok(())
            }
            Err(e) => Err(format!("{e}")),
        }
    }



    fn play_recent(&mut self, source_id: i64, path: PathBuf, http_headers: Option<String>) {
        // 拖拽播放（临时播放）的历史记录：本地文件按同目录组队恢复。
        if source_id == drop_play::DROP_SOURCE_ID {
            // 串流 URL（http/rtmp/rtsp 等）直接开播，本地文件走同目录组队恢复。
            let path_str = path.to_string_lossy();
            if is_stream_url(&path_str) {
                let pending = player_app::PendingPlay {
                    source_id: 0,
                    items: vec![player_app::MediaItem {
                        name: path_str.into_owned(),
                        path: path.clone(),
                        size: 0,
                        subs: vec![],
                    }],
                    start_index: 0,
                    remote: None,
                    http_headers,
                };
                let ctx = self.egui_ctx.clone();
                self.start_play(&ctx, pending);
            } else {
                self.play_recent_dropped(path);
            }
            return;
        }
        let Some(source) = self.sources.iter().find(|s| s.id == source_id).cloned() else {
            self.notice = Some("该记录所属的源已不存在".into());
            return;
        };
        let parent = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| source.root.clone());
        let ctx = self.egui_ctx.clone();
        self.browser
            .request_folder_play(&source, parent, Some(path), &ctx);
    }

    /// 最近播放中的拖拽记录恢复：文件仍存在则后台同目录组队开播（自动续播走既有机制）。
    /// 组队（canonicalize + 目录扫描）派后台线程，结果经 pending_drop 回流。
    fn play_recent_dropped(&mut self, path: PathBuf) {
        if !path.is_file() {
            self.notice = Some("该文件已不存在".into());
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("dyplayer-recent".into())
            .spawn(move || {
                let plan = match drop_play::playlist_for_single_video(&path) {
                    Ok(pending) => drop_play::DropPlayPlan {
                        pending: Some(pending),
                        ..Default::default()
                    },
                    Err(e) => drop_play::DropPlayPlan {
                        errors: vec![format!("恢复播放失败: {e}")],
                        ..Default::default()
                    },
                };
                let _ = tx.send(plan);
            })
            .ok();
        self.pending_drop = Some(rx);
    }

    /// 拖放分流入口（Browse/Player/空态播放页共用）：
    /// classify 留主线程（轻），build_drop_playlist 派后台线程（canonicalize +
    /// 目录扫描重 IO），结果经 channel 回流由 ui() poll 后 apply_drop_plan 消费。
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let items = drop_play::classify_dropped(dropped);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("dyplayer-drop".into())
            .spawn(move || {
                let plan = drop_play::build_drop_playlist(&items);
                let _ = tx.send(plan);
            })
            .ok();
        self.pending_drop = Some(rx);
    }

    /// 消费后台组队结果：start_play / XML 弹幕应用 / 通知组装。
    fn apply_drop_plan(&mut self, ctx: &egui::Context, plan: drop_play::DropPlayPlan) {
        let mut notices: Vec<String> = Vec::new();
        if !plan.ignored.is_empty() {
            notices.push(format!("已忽略 {} 个不支持的文件", plan.ignored.len()));
        }
        if plan.extra_xmls > 0 {
            notices.push(format!("已忽略 {} 个多余的弹幕文件", plan.extra_xmls));
        }
        notices.extend(plan.errors.iter().cloned());

        let has_video = plan.pending.is_some();
        if let Some(pending) = plan.pending {
            self.start_play(ctx, pending);
        }

        if let Some(xml) = plan.xml {
            match &mut self.player {
                // 同帧拖入视频+弹幕：等 FileLoaded（同名自动加载）之后再应用，覆盖同名轨。
                Some(player) if has_video => {
                    player.queue_danmaku_file(xml);
                }
                Some(player) => match player.load_danmaku_file(&xml) {
                    Ok(n) => notices.push(format!("已载入 {n} 条弹幕")),
                    Err(e) => notices.push(e),
                },
                None => notices.push("请先播放视频再载入弹幕".into()),
            }
        }

        if !notices.is_empty() {
            self.notice = Some(notices.join("；"));
        }
    }

    /// 收藏夹点击文件夹：切到该源并浏览该目录。
    fn open_favorite_dir(&mut self, source_id: i64, dir: PathBuf) {
        let Some(idx) = self.sources.iter().position(|s| s.id == source_id) else {
            self.notice = Some("该收藏所属的源已不存在".into());
            return;
        };
        let Some(source) = self.sources.get(idx).cloned() else {
            return;
        };
        self.browser.set_active_source(Some(idx), &self.sources);
        self.browser.view = BrowserView::Files;
        self.browser.refresh_favorites(&self.store);
        let ctx = self.egui_ctx.clone();
        self.browser.request_dir(&source, dir, &ctx);
    }
}

impl eframe::App for DyPlayerApp {
    /// 窗口不可见期间（全屏切换的样式变换瞬间、最小化等）eframe 不运行 `ui()`，
    /// 若无任何 repaint 请求，事件循环会以 INFINITE 休眠导致界面永久冻结
    /// （暂停时点全屏即触发：mpv 不再产帧唤醒，调度表被消费殆尽）。
    /// 这里周期性自续 repaint 心跳，保证隐藏期间事件循环持续运转、
    /// `logic`/`ui` 在窗口恢复可见后正常接管。
    /// 副作用：暂停且可见时 UI 以 ~5fps 低频刷新（HUD 到期隐藏等得以生效，有益）。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        #[cfg(not(feature = "backend-wgpu"))]
        {
            if _visuals.dark_mode {
                [0.0, 0.0, 0.0, 1.0]
            } else {
                [0.90, 0.90, 0.90, 1.0]
            }
        }
        #[cfg(feature = "backend-wgpu")]
        {
            egui::Color32::from_rgba_unmultiplied(12, 12, 12, 180).to_normalized_gamma_f32()
        }
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 隐藏创建窗口的首次显示（闪白修复）：隐藏期间 eframe 只跑 logic
        // 不跑 ui pass——显示前在此装好无边框样式（prime），再 Win32 直调
        // ShowWindow（egui Visible 命令链实测延迟秒级，勿改回命令链）。
        if !self.viewport_shown {
            self.viewport_shown = true;
            fs_patch::prime_borderless(frame);
            fs_patch::show_main_window();

        }
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // R20A：从 PlayerSession 同步 engine/player config 镜像，
        // 避免媒体库保存时用 App 旧快照覆盖播放页刚写入的字段。
        if let Some(player) = &self.player {
            self.config.engine = player.engine_config().clone();
            self.config.player = player.player_config().clone();
        }
        // 窗口所在屏精确刷新率（首帧检测一次并缓存；mpv display 配速注入用）
        refresh::prime(frame);
        // 常态无边框（首帧一次）：装饰创建 + THICKFRAME+NCCALCSIZE 自管去装饰。
        // 勿改回 with_decorations(false)——Win10 虚拟机（VMware SVGA3D）上
        // winit 剥光样式会导致 DWM 呈现失效、客户区全黑（最小 demo 已坐实）。
        fs_patch::prime_borderless(frame);

        // 每帧校验：winit refresh（启动风暴/DPI/resize 事件）会把 WS_CAPTION
        // 写回，被冲掉即恢复。无变化时仅一次 GetWindowLongW（微秒级）。
        fs_patch::maintain_borderless();
        self.probe
            .frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(player) = &mut self.player {
            player.poll_events();
        }
        // 解码方式切换（会话重建型）：播放页设置卡片只改 engine 镜像并发
        // 请求，此处按新 hwdec 重建会话——新解码器/VO/render context 从
        // 关键帧起播，避开运行时改 hwdec 留下的持久横纹/色块。续位/暂停态
        // 与全屏状态由 rebuild_session 接管。
        if self
            .player
            .as_mut()
            .is_some_and(|p| p.take_hwdec_rebuild_request())
        {
            if let Some(player) = &self.player {
                self.config.engine = player.engine_config().clone();
            }
            tracing::info!(
                "解码方式切换: hwdec={}（会话重建型）",
                self.config.engine.hwdec
            );
            self.save_engine_config();
            self.rebuild_session(ui.ctx(), "解码方式已切换", true);
        }

        // 媒体索引每日刷新：启动 8s 后（避开首帧/源加载高峰）单线程逐源
        // 重建（能连通的源；连不通的源保留旧索引并 warn）。完成后落
        // last_index_date，当天不再重复。后台执行不阻塞 UI。
        if self.index_refresh_pending
            && !self.index_refresh_started
            && self.startup_at.elapsed() > std::time::Duration::from_secs(8)
        {
            self.index_refresh_started = true;
            let sources = self.sources.clone();
            let store = Arc::clone(&self.store);
            let ctx = self.egui_ctx.clone();
            std::thread::Builder::new()
                .name("dyplayer-reindex".into())
                .spawn(move || {
                    for source in &sources {
                        match browser::reindex_source(source, &store) {
                            Some(n) => {
                                tracing::info!("媒体索引已更新：{}（{n} 条）", source.name)
                            }
                            None => {
                                tracing::warn!("媒体索引跳过（无法连通）：{}", source.name)
                            }
                        }
                        ctx.request_repaint();
                    }
                    let today = store.local_date();
                    if !today.is_empty()
                        && let Err(e) = store.set_meta("last_index_date", &today)
                    {
                        tracing::warn!("写入索引日期失败: {e}");
                    }
                    ctx.request_repaint();
                })
                .ok();
        }

        // 全局明暗主题（跟随系统/浅色/深色）：只随用户偏好变化，与 screen
        // 无关——历史方案让全局主题跟随 screen（播放页固定深色），每次切屏
        // 都是一次全局主题切换，且空态播放页的 early return 会跳过帧末应用，
        // 导致切屏首帧按旧主题快照渲染（媒体库闪一帧深色）。播放页的固定
        // 深色改由分支内局部 visuals override 实现（帧内即时生效，无
        // begin_pass 快照时延），全局主题只在设置改动时变化。
        if let Some(player) = &self.player {
            self.theme_mode = player.ui_theme();
        }
        let dark = theme::effective_dark(self.theme_mode);
        let want = if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };

        if self.egui_ctx.theme() != want {
            self.egui_ctx.set_theme(want);
            // set_theme 重置为默认 style，强制重映射语义色。
            self.style_applied = None;
        }
        // 全局主题色（验证页设置视图 / config [ui].accent）：把 Palette 语义
        // 色映射进 egui Style（标准件与自绘 UI 观感统一），仅快照变化时执行。
        if self.style_applied != Some((dark, self.config.ui.accent)) {
            theme::apply_style(&self.egui_ctx, dark, self.config.ui.accent as usize);
            self.style_applied = Some((dark, self.config.ui.accent));
        }

        // 拖放组队后台回流 poll：拿到 plan 后执行 start_play / XML / 通知。
        if let Some(rx) = &self.pending_drop {
            match rx.try_recv() {
                Ok(plan) => {
                    self.pending_drop = None;
                    let ctx = ui.ctx().clone();
                    self.apply_drop_plan(&ctx, plan);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending_drop = None;
                }
            }
        }

        // 拖放处理。
        {
            let ctx = ui.ctx().clone();
            self.handle_dropped_files(&ctx);
        }

        // 自管全屏（fs_patch）时隐藏标题栏与边缘 resize。
        let fullscreen = self.player.as_ref().is_some_and(|p| p.is_fullscreen());

        match self.screen {
            Screen::Player => {
                // 本页固定深色（视频舞台黑底）：局部 visuals override，帧内
                // 即时生效，且不污染全局主题（切到媒体库零闪变）。
                *ui.visuals_mut() = egui::Visuals::dark();
                // 播放页标题栏常驻深色：视频区黑底（空态也是黑），不随全局明暗切换。
                if !fullscreen {
                    titlebar::chrome(ui, titlebar::TitleBarStyle::dark());
                }
                let Some(player) = &mut self.player else {
                    // 空态播放页（启动默认进播放页 / 从未播放）。
                    self.ui_player_empty(ui);
                    return;
                };
                player.ui(ui, frame);
                if player.requested_exit {
                    // 退回媒体库前若仍在播放则先暂停；返回播放页时不
                    // 触碰状态（保持暂停），新开播统一重置为播放态。
                    player.pause_if_playing();
                    // GPU 宿主/覆盖层收起（屏幕外/隐藏）：WS_CHILD 永远浮在
                    // 父窗口内容之上，不收走会残留悬浮在媒体库内容上。
                    player.set_player_screen_active(false);
                    self.screen = Screen::Browse;
                    player.requested_exit = false;
                    // 切换页面主动刷新（替代原 2s 定时轮询）。
                    self.browse_ui.ensure_data(&self.store, &mut self.browser);
                    self.browse_ui.sync_ui_cfg(&self.config.ui, &self.config.engine);
                }
            }

            Screen::Browse => {
                self.probe.set_phase(PH_IDLE);
                let ctx = ui.ctx().clone();
                // Browse 主界面 = browse_ui shell（卡片化布局 +
                // 五导航视图 + 设置）。底色先铺满（含标题栏区域），标题栏
                // 同色画在上面，卡片浮于其上——顺序反了会把标题栏盖掉。
                let bar_style = browse_ui::titlebar_style(&self.browse_ui, dark);
                ui.painter()
                    .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, bar_style.bg);
                titlebar::chrome(ui, bar_style);

                browse_ui::ui(
                    ui,
                    &mut self.browse_ui,
                    &self.sources,
                    &self.browser,
                    &self.store,
                    &mut self.theme_mode,
                    dark,
                );
                // 侧栏切换 tab 触发刷新（替代原 2s 定时轮询）。
                if self.browse_ui.pending_refresh {
                    self.browse_ui.pending_refresh = false;
                    self.browse_ui.ensure_data(&self.store, &mut self.browser);
                }
                // 后台目录请求结果回流（含"播放文件夹/最近播放"完成事件）。
                if let Some(pending) = self.browser.poll_dir(&ctx) {
                    self.start_play(&ctx, pending);
                }
                // 媒体库集数统计回流：逐条写入缓存（UI 下帧取用）。
                if let Some(rx) = &self.lib_stats_rx {
                    loop {
                        match rx.try_recv() {
                            Ok((id, stats)) => {
                                self.browse_ui.lib_stats.insert(id, stats);
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                ctx.request_repaint_after(std::time::Duration::from_millis(80));
                                break;
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                self.lib_stats_rx = None;
                                break;
                            }
                        }
                    }
                }
                // 待统计条目批量派发（单线程逐条处理；上一批未收完则推回
                // 下帧重试，保持 request_lib_stats 的去重语义）。
                let batch = std::mem::take(&mut self.browse_ui.pending_lib_stats);
                if !batch.is_empty() {
                    if self.lib_stats_rx.is_some() {
                        self.browse_ui.pending_lib_stats = batch;
                    } else {
                        let sources = self.sources.clone();
                        let store = Arc::clone(&self.store);
                        let spawn_ctx = self.egui_ctx.clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::Builder::new()
                            .name("dyplayer-lib-stats".into())
                            .spawn(move || {
                                for (lib_id, source_id, folder) in batch {
                                    let stats = browser::compute_episode_stats(
                                        &sources, &store, source_id, &folder,
                                    );
                                    let _ = tx.send((lib_id, stats));
                                    spawn_ctx.request_repaint();
                                }
                            })
                            .ok();
                        self.lib_stats_rx = Some(rx);
                    }
                }
                // 封面纹理回流：逐条 ctx.load_texture → cover_cache（UI 下帧取用）。
                if let Some(rx) = &self.cover_load_rx {
                    loop {
                        match rx.try_recv() {
                            Ok((path, Some(img))) => {
                                let tex = ctx.load_texture(
                                    path.clone(),
                                    img,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.browse_ui.cover_cache.insert(
                                    path,
                                    browse_ui::CoverState::Ready(tex),
                                );
                            }
                            Ok((path, None)) => {
                                self.browse_ui
                                    .cover_cache
                                    .insert(path, browse_ui::CoverState::Failed);
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                ctx.request_repaint_after(std::time::Duration::from_millis(80));
                                break;
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                self.cover_load_rx = None;
                                break;
                            }
                        }
                    }
                }
                // 待加载封面批量派发（单线程逐个 VFS read_full + image 解码；
                // 上一批未收完则推回下帧重试，保持 pending_cover_load 去重语义）。
                let batch = std::mem::take(&mut self.browse_ui.pending_cover_load);
                if !batch.is_empty() {
                    if self.cover_load_rx.is_some() {
                        self.browse_ui.pending_cover_load = batch;
                    } else {
                        let sources = self.sources.clone();
                        let spawn_ctx = self.egui_ctx.clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::Builder::new()
                            .name("dyplayer-cover-load".into())
                            .spawn(move || {
                                for (cover_path, source_id) in batch {
                                    let img = sources
                                        .iter()
                                        .find(|s| s.id == source_id)
                                        .and_then(|source| source.open_vfs().ok())
                                        .and_then(|(vfs, _)| {
                                            vfs.read_full(std::path::Path::new(&cover_path))
                                                .ok()
                                        })
                                        .and_then(|bytes| thumb_gen::decode_image(&bytes));
                                    let _ = tx.send((cover_path, img));
                                    spawn_ctx.request_repaint();
                                }
                            })
                            .ok();
                        self.cover_load_rx = Some(rx);
                    }
                }
                // 抽帧缩略图纹理回流：逐条 ctx.load_texture → thumb_cache。
                if let Some(rx) = &self.thumb_gen_rx {
                    loop {
                        match rx.try_recv() {
                            Ok(result) => {
                                if let Some(img) = result.image {
                                    let tex = ctx.load_texture(
                                        result.key.clone(),
                                        img,
                                        egui::TextureOptions::LINEAR,
                                    );
                                    self.browse_ui.thumb_cache.insert(
                                        result.key,
                                        browse_ui::CoverState::Ready(tex),
                                    );
                                } else {
                                    self.browse_ui
                                        .thumb_cache
                                        .insert(result.key, browse_ui::CoverState::Failed);
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                ctx.request_repaint_after(std::time::Duration::from_millis(80));
                                break;
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                self.thumb_gen_rx = None;
                                break;
                            }
                        }
                    }
                }
                // 待抽帧缩略图批量派发（单线程逐个 headless mpv 抽帧；
                // 上一批未收完则推回下帧重试，保持 pending_thumb_gen 去重语义）。
                let thumb_batch = std::mem::take(&mut self.browse_ui.pending_thumb_gen);
                if !thumb_batch.is_empty() {
                    if self.thumb_gen_rx.is_some() {
                        self.browse_ui.pending_thumb_gen = thumb_batch;
                    } else {
                        let sources = self.sources.clone();
                        let spawn_ctx = self.egui_ctx.clone();
                        let libmpv_path = self.libmpv_path.clone();
                        let thumbs_dir = dyplayer_store::AppConfig::data_dir()
                            .map(|d| d.join("thumbs"))
                            .unwrap_or_else(|_| std::path::PathBuf::from("thumbs"));
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::Builder::new()
                            .name("dyplayer-thumb-gen".into())
                            .spawn(move || {
                                for (source_id, folder_path) in thumb_batch {
                                    let key = thumb_gen::thumb_key(source_id, &folder_path);
                                    let Some(source) =
                                        sources.iter().find(|s| s.id == source_id)
                                    else {
                                        let _ = tx.send(thumb_gen::ThumbResult {
                                            key,
                                            image: None,
                                        });
                                        spawn_ctx.request_repaint();
                                        continue;
                                    };
                                    let req = thumb_gen::ThumbRequest {
                                        source_id,
                                        folder_path: folder_path.clone(),
                                        source: source.clone(),
                                        libmpv_path: libmpv_path.clone(),
                                        thumbs_dir: thumbs_dir.clone(),
                                    };
                                    let img = thumb_gen::extract_thumb(&req);
                                    let _ = tx.send(thumb_gen::ThumbResult { key, image: img });
                                    spawn_ctx.request_repaint();
                                }
                            })
                            .ok();
                        self.thumb_gen_rx = Some(rx);
                    }
                }
                // 侧栏点击源：激活 + 清缓存 + 请求该源根目录。
                // ui() 内只读借用 browser，可变操作集中到帧末处理。
                if let Some(idx) = self.browse_ui.pending_source.take() {
                    self.browser.view = BrowserView::Files;
                    self.browser.set_active_source(Some(idx), &self.sources);
                    self.browser.entries.clear();
                    self.browser.dir = PathBuf::new();
                    self.request_active_root();
                }
                // Shell 真实交互动作（开播 / 跳目录 / 源保存）：帧末集中执行，
                // 开播为异步组队，完成事件经上方 poll_dir 回流后切播放页。
                for action in std::mem::take(&mut self.browse_ui.shell_actions) {
                    match action {
                        browse_ui::ShellAction::ResumeRecent { source_id, path, http_headers } => {
                            self.play_recent(source_id, path, http_headers);
                        }
                        browse_ui::ShellAction::PlayFolder { source_id, dir } => {
                            self.request_folder_play(source_id, dir, None, &ctx);
                        }
                        browse_ui::ShellAction::PlayFile {
                            source_id,
                            dir,
                            file,
                        } => {
                            self.request_folder_play(source_id, dir, Some(file), &ctx);
                        }
                        browse_ui::ShellAction::OpenFolder { source_id, dir } => {
                            self.open_favorite_dir(source_id, dir);
                            // 侧栏选中态即时同步（不等 2s 节流刷新）。
                            self.browse_ui.selected_source = self.browser.active_source;
                        }
                        browse_ui::ShellAction::BrowseInto { dir } => {
                            self.request_browse_into(dir);
                        }
                        browse_ui::ShellAction::BrowseRoot => {
                            self.request_active_root();
                        }
                        browse_ui::ShellAction::AddSourceLocal => {
                            if let Some(dir) = rfd::FileDialog::new()
                                .set_title("选择本地媒体文件夹")
                                .pick_folder()
                            {
                                self.add_local_source(dir);
                                self.browse_ui.selected_source = self.browser.active_source;
                            }
                        }
                        browse_ui::ShellAction::AddSourceRemote(kind) => {
                            // 与编辑源同一定制弹框（新增模式）。
                            self.browse_ui.open_add_source(kind);
                        }
                        browse_ui::ShellAction::ToggleFavorite {
                            source_id,
                            path,
                            title,
                            is_dir,
                        } => {
                            match self.store.toggle_favorite(
                                source_id,
                                &path.to_string_lossy(),
                                &title,
                                is_dir,
                            ) {
                                Ok(true) => {
                                    self.browse_ui.notify(format!("已收藏“{title}”"))
                                }
                                Ok(false) => {
                                    self.browse_ui.notify(format!("已取消收藏“{title}”"))
                                }
                                Err(e) => self.browse_ui.notify(format!("收藏操作失败: {e}")),
                            }
                            self.browser.refresh_favorites(&self.store);
                        }
                        browse_ui::ShellAction::SaveSource(form) => {
                            // source_id None/Some 分派：新建走 add（落库后激活
                            // 新源），编辑走 update（活跃源时刷新目录）。
                            match form.source_id {
                                None => self.add_remote_source(*form),
                                Some(id) => self.update_remote_source(id, *form),
                            }
                            // 侧栏选中态即时同步（新增/编辑后激活源可能变化）。
                            self.browse_ui.selected_source = self.browser.active_source;
                        }
                        browse_ui::ShellAction::DeleteSource { source_id, name } => {
                            self.delete_source(source_id, name);
                        }
                        browse_ui::ShellAction::AddToLib {
                            source_id,
                            folder_path,
                            name,
                        } => {
                            // 后台异步：list_dir 子文件夹探测 cover.jpg 约定封面
                            //（request_dir 的 is_listable_entry 已滤掉 cover.jpg，
                            // 必须独立 list_dir 未过滤 entries），再 add_media_library
                            // 一次完成；request_repaint 触发下帧刷新。
                            let Some(source) =
                                self.sources.iter().find(|s| s.id == source_id).cloned()
                            else {
                                self.browse_ui
                                    .notify("源不存在，加入媒体库失败".to_string());
                                continue;
                            };
                            let store = Arc::clone(&self.store);
                            let ctx = self.egui_ctx.clone();
                            std::thread::Builder::new()
                                .name("dyplayer-add-lib".into())
                                .spawn(move || {
                                    let cover = match source.open_vfs() {
                                        Ok((vfs, _)) => match vfs.list_dir(&folder_path) {
                                            Ok(entries) => {
                                                let cover = browse_ui::detect_cover(&entries);
                                                if cover.is_none() {
                                                    tracing::warn!(
                                                        "AddToLib: {} list_dir 返回 {} 条，未找到约定封面（cover/poster/folder.jpg|png）",
                                                        folder_path.display(),
                                                        entries.len()
                                                    );
                                                }
                                                cover
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "AddToLib: {} list_dir 失败: {e}",
                                                    folder_path.display()
                                                );
                                                None
                                            }
                                        },
                                        Err(e) => {
                                            tracing::warn!(
                                                "AddToLib: 源「{}」open_vfs 失败: {e}",
                                                source.name
                                            );
                                            None
                                        }
                                    };
                                    let folder = folder_path.to_string_lossy().into_owned();
                                    match store.add_media_library(
                                        source_id,
                                        &folder,
                                        &name,
                                        cover.as_deref(),
                                    ) {
                                        Ok(()) => ctx.request_repaint(),
                                        Err(e) => tracing::warn!("加入媒体库失败: {e}"),
                                    }
                                })
                                .ok();
                        }
                        browse_ui::ShellAction::ClearHistory(target) => {
                            let result = match &target {
                                browse_ui::ClearHistoryTarget::Single {
                                    source_id,
                                    path,
                                    ..
                                } => self.store.delete_play_history(*source_id, path),
                                browse_ui::ClearHistoryTarget::All => {
                                    self.store.clear_play_history()
                                }
                                browse_ui::ClearHistoryTarget::AllSingles {
                                    lib_folder_paths,
                                } => self
                                    .store
                                    .clear_play_history_for_singles(lib_folder_paths),
                                browse_ui::ClearHistoryTarget::AllLibs {
                                    lib_folder_paths,
                                } => self
                                    .store
                                    .clear_play_history_for_libs(lib_folder_paths),
                                browse_ui::ClearHistoryTarget::Folder {
                                    folder_path, ..
                                } => self
                                    .store
                                    .clear_play_history_for_folder(folder_path),
                            };
                            match result {
                                Ok(n) => {
                                    self.browse_ui
                                        .notify(format!("已清除 {n} 条播放记录"));
                                    self.browser.refresh_recent(&self.store);
                                }
                                Err(e) => {
                                    self.browse_ui
                                        .notify(format!("清除播放记录失败: {e}"));
                                    tracing::warn!("清除播放记录失败: {e}");
                                }
                            }
                        }
                    }
                }
                // 后台目录/开播请求失败：桥接进 shell notice（成功开播
                // 直接切播放页，无需提示）。
                if let Some(err) = self.browser.error.take() {
                    self.browse_ui.notify(err);
                }
                // 设置视图的待写回项：写内存 config + 落盘；字体即时重装
                //（set_fonts 全量重绘），明暗回流播放会话镜像（帧首同步不回退）。
                for change in std::mem::take(&mut self.browse_ui.pending_ui) {
                    match change {
                        browse_ui::PendingUiChange::Theme(mode) => {
                            self.config.ui.theme = mode;
                            self.theme_mode = mode;
                            if let Some(player) = &mut self.player {
                                player.set_ui_theme(mode);
                            }
                        }
                        browse_ui::PendingUiChange::Accent(idx) => {
                            self.config.ui.accent = idx;
                        }
                        browse_ui::PendingUiChange::Font(font) => {
                            self.config.ui.font = font.clone();
                            player_app::install_cjk_font(&ctx, &font);
                        }
                        browse_ui::PendingUiChange::Startup(view) => {
                            self.config.ui.startup_view = view;
                        }
                        // 渲染路径（从媒体库公共设置切换，会话重建型）：
                        // 写内存 + 引擎段落盘；有活跃会话则 drop 后按新
                        // 配置重建并续位进度/暂停态——留在媒体库页不跳屏
                        //（GPU 宿主收起，回播放页首帧 align 重父拉回）。
                        browse_ui::PendingUiChange::RenderPath(path) => {
                            if self.config.engine.render_path.eq_ignore_ascii_case(&path) {
                                continue;
                            }
                            tracing::info!(
                                "渲染路径切换: render_path={path}（会话重建型，媒体库设置入口）"
                            );
                            self.config.engine.render_path = path;
                            self.save_engine_config();
                            self.rebuild_session(&ctx, "渲染路径已切换，返回播放页生效", false);
                            continue;
                        }
                        // 画质档位（会话重建型）：mpv 初始化期 scale/cscale/
                        // interpolation 选项，运行时改需重建会话续位。
                        browse_ui::PendingUiChange::QualityPreset(preset) => {
                            if self.config.engine.quality_preset == preset {
                                continue;
                            }
                            tracing::info!("画质档位切换: {preset}（会话重建型）");
                            self.config.engine.quality_preset = preset;
                            self.save_engine_config();
                            self.rebuild_session(&ctx, "画质档位已切换，返回播放页生效", false);
                            continue;
                        }
                        // 引擎高级参数（会话重建型）：mpv 初始化期选项，
                        // 运行时改需重建会话续位。空列表也允许（清空全部参数）。
                        browse_ui::PendingUiChange::ExtraArgs(args) => {
                            if self.config.engine.extra_args == args {
                                continue;
                            }
                            tracing::info!("引擎高级参数切换: {} 项（会话重建型）", args.len());
                            self.config.engine.extra_args = args;
                            self.save_engine_config();
                            self.rebuild_session(&ctx, "高级参数已更新，返回播放页生效", false);
                            continue;
                        }
                    }
                    self.save_ui_config();
                }

                // 顶层错误/提示桥接进 shell notice（拖放反馈 / 播放引擎错误等）。
                if let Some(msg) = self.notice.take() {
                    self.browse_ui.notify(msg);
                }

                // "返回播放页 (ESC)"：回播放页（无会话时是空态引导页）。
                if self.browse_ui.exit_requested {
                    self.browse_ui.exit_requested = false;
                    self.browse_ui.reset_transient();
                    self.screen = Screen::Player;
                    // GPU 宿主/覆盖层恢复：宿主由下一帧 ui() 的 align 拉回
                    // （park 已作废对齐缓存），覆盖层置回可见。
                    if let Some(player) = &mut self.player {
                        player.set_player_screen_active(true);
                    }
                }
            }
        }
    }


    #[cfg(feature = "backend-wgpu")]
    fn on_exit(&mut self) {
        if let Some(player) = &mut self.player {
            player.on_exit();
        }
    }
    #[cfg(not(feature = "backend-wgpu"))]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(player) = &mut self.player {
            player.on_exit();
        }
    }
}

impl DyPlayerApp {
    /// 将界面设置段写回 config.toml（load → 改 ui 段 → save，与其他段
    /// 合并保存；与播放页 save_player_config 同模式，避免覆盖他段改动）。
    fn save_ui_config(&self) {
        let Ok(dir) = dyplayer_store::AppConfig::config_dir() else {
            return;
        };
        match dyplayer_store::AppConfig::load_or_default() {
            Ok(mut cfg) => {
                cfg.ui = self.config.ui.clone();
                if let Err(e) = cfg.save(&dir.join("config.toml")) {
                    tracing::warn!("保存界面配置失败: {e}");
                }
            }
            Err(e) => tracing::warn!("读取配置失败，无法保存界面设置: {e}"),
        }
    }

    /// 将引擎设置段写回 config.toml（load → 改 engine 段 → save；与
    /// save_ui_config 同模式，避免覆盖他段改动）。
    fn save_engine_config(&self) {
        let Ok(dir) = dyplayer_store::AppConfig::config_dir() else {
            return;
        };
        match dyplayer_store::AppConfig::load_or_default() {
            Ok(mut cfg) => {
                cfg.engine = self.config.engine.clone();
                if let Err(e) = cfg.save(&dir.join("config.toml")) {
                    tracing::warn!("保存引擎配置失败: {e}");
                }
            }
            Err(e) => tracing::warn!("读取配置失败，无法保存引擎设置: {e}"),
        }
    }

    /// 会话重建（渲染路径/画质档位/高级参数/解码方式切换用）：drop 现会话后
    /// 按新配置重建并续位进度/暂停态。`keep_active`：从播放页内部触发时为
    /// true——保持播放页（含全屏）状态，不把 GPU 宿主收走；媒体库设置入口
    /// 为 false（留在媒体库页不跳屏，回播放页首帧 align 重父拉回）。
    fn rebuild_session(&mut self, ctx: &egui::Context, notify: &str, keep_active: bool) {
        if let Some(player) = self.player.take() {
            let (pending, pos, paused) = player.pending_for_rebuild();
            let fullscreen = player.is_fullscreen();
            drop(player);
            let ctx2 = ctx.clone();
            match self.create_player_session(&ctx2, pending) {
                Ok(()) => {
                    if let Some(new_player) = &mut self.player {
                        new_player.set_pending_resume(pos, paused);
                        if keep_active {
                            if fullscreen {
                                new_player.set_fullscreen(true);
                            }
                        } else {
                            new_player.set_player_screen_active(false);
                        }
                    }
                    self.browse_ui.notify(notify);
                }
                Err(e) => {
                    self.notice = Some(format!("重建播放会话失败: {e}"));
                }
            }
        }
    }

    /// 空态播放页：无播放会话时展示（启动默认进播放页），引导拖放或返回媒体库。
    fn ui_player_empty(&mut self, ui: &mut egui::Ui) {
        self.probe.set_phase(PH_IDLE);
        // 空状态 Ctrl+V 粘贴链接开播。
        self.handle_paste_stream(ui.ctx());
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.35);
                    ui.label(
                        egui::RichText::new("拖入视频文件或文件夹开始播放")
                            .size(20.0)
                            .color(egui::Color32::from_gray(170)),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("支持多个文件与文件夹混拖 · 播放中可拖入 XML 弹幕文件")
                            .size(13.0)
                            .color(egui::Color32::from_gray(120)),
                    );
                    ui.add_space(20.0);
                    if self.ui_library_pill(ui) {
                        self.screen = Screen::Browse;
                        self.browse_ui.ensure_data(&self.store, &mut self.browser);
                        self.browse_ui.sync_ui_cfg(&self.config.ui, &self.config.engine);
                    }
                    ui.add_space(10.0);
                    if self.ui_stream_pill(ui) {
                        self.stream_dialog_open = true;
                    }
                });
            });
        if self.stream_dialog_open {
            self.ui_stream_dialog(ui);
        }
    }

    /// OSC 风格媒体库胶囊按钮（与 osc.rs library_button 同款：半透明白底
    /// + 白描边 + house.svg 图标 + 文本，hover 0.08s 提亮）。返回是否点击。
    fn ui_library_pill(&mut self, ui: &mut egui::Ui) -> bool {
        crate::player_app::osc::install_svg_icons(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(120.0, 30.0), egui::Sense::click());
        let t = ui.ctx().animate_value_with_time(
            egui::Id::unique("empty-library-pill"),
            resp.hovered() as u8 as f32,
            0.08,
        );
        let radius = (rect.height() / 2.0) as u8;
        let white =
            |a: f32| egui::Color32::from_rgba_unmultiplied(255, 255, 255, (a * 255.0) as u8);
        let p = ui.painter().clone();
        p.rect_filled(
            rect,
            egui::CornerRadius::same(radius),
            white(0.10 + 0.08 * t),
        );
        p.rect_stroke(
            rect,
            egui::CornerRadius::same(radius),
            egui::Stroke::new(1.2, white(0.50 + 0.25 * t)),
            egui::StrokeKind::Inside,
        );
        let icon_sz = 15.0;
        let gap = 6.0;
        let galley = p.layout_no_wrap(
            "打开媒体库".to_owned(),
            egui::FontId::new(13.0, egui::FontFamily::Name("HarmonyOS Sans SC".into())),
            white(0.92),
        );
        let content_w = icon_sz + gap + galley.size().x;
        let cx = rect.center().x - content_w / 2.0;
        let cy = rect.center().y;
        let _ = ui.put(
            egui::Rect::from_center_size(
                egui::pos2(cx + icon_sz / 2.0, cy),
                egui::vec2(icon_sz, icon_sz),
            ),
            crate::player_app::osc::house_svg_image(white(0.92), icon_sz),
        );
        p.galley(
            egui::pos2(cx + icon_sz + gap, cy - galley.size().y / 2.0),
            galley,
            egui::Color32::WHITE,
        );

        resp.clicked()
    }

    /// 串流播放胶囊按钮（与 ui_library_pill 同款工艺，link.svg 图标）。
    fn ui_stream_pill(&mut self, ui: &mut egui::Ui) -> bool {
        crate::player_app::osc::install_svg_icons(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(120.0, 30.0), egui::Sense::click());
        let t = ui.ctx().animate_value_with_time(
            egui::Id::unique("empty-stream-pill"),
            resp.hovered() as u8 as f32,
            0.08,
        );
        let radius = (rect.height() / 2.0) as u8;
        let white =
            |a: f32| egui::Color32::from_rgba_unmultiplied(255, 255, 255, (a * 255.0) as u8);
        let p = ui.painter().clone();
        p.rect_filled(rect, egui::CornerRadius::same(radius), white(0.10 + 0.08 * t));
        p.rect_stroke(
            rect,
            egui::CornerRadius::same(radius),
            egui::Stroke::new(1.2, white(0.50 + 0.25 * t)),
            egui::StrokeKind::Inside,
        );
        let icon_sz = 15.0;
        let gap = 6.0;
        let galley = p.layout_no_wrap(
            "在线串流".to_owned(),
            egui::FontId::new(13.0, egui::FontFamily::Name("HarmonyOS Sans SC".into())),
            white(0.92),
        );
        let content_w = icon_sz + gap + galley.size().x;
        let cx = rect.center().x - content_w / 2.0;
        let cy = rect.center().y;
        let _ = ui.put(
            egui::Rect::from_center_size(
                egui::pos2(cx + icon_sz / 2.0, cy),
                egui::vec2(icon_sz, icon_sz),
            ),
            crate::player_app::osc::link_svg_image(white(0.92), icon_sz),
        );
        p.galley(
            egui::pos2(cx + icon_sz + gap, cy - galley.size().y / 2.0),
            galley,
            egui::Color32::WHITE,
        );
        resp.clicked()
    }

    /// 串流播放弹窗（Foreground 层，样式同弹幕设置）。
    fn ui_stream_dialog(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let screen_rect = ctx.content_rect();
        // 遮罩：全屏半透明 + 点击关闭。
        let mask_resp = egui::Area::new(egui::Id::unique("stream-dialog-mask"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen_rect.min)
            .show(&ctx, |ui| {
                let (rect, resp) =
                    ui.allocate_exact_size(screen_rect.size(), egui::Sense::click());
                ui.painter().rect_filled(
                    rect,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_black_alpha(120),
                );
                resp
            });
        // 弹窗卡片。
        let dialog_pos = screen_rect.center() - egui::vec2(220.0, 140.0);
        let dialog_resp = egui::Area::new(egui::Id::unique("stream-dialog"))
            .order(egui::Order::Foreground)
            .fixed_pos(dialog_pos)
            .show(&ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(0x1C, 0x1C, 0x22, 235))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                    ))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.set_width(400.0);
                        ui.label(
                            egui::RichText::new("在线串流")
                                .size(16.0)
                                .color(egui::Color32::WHITE),
                        );
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("链接")
                                .size(12.0)
                                .color(egui::Color32::from_gray(170)),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.stream_url)
                                .hint_text("http:// / rtmp:// / rtsp://")
                                .desired_width(400.0),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("请求头（每行 Key: Value，可选）")
                                .size(12.0)
                                .color(egui::Color32::from_gray(170)),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::multiline(&mut self.stream_headers)
                                .hint_text("Referer: https://example.com\nUser-Agent: Mozilla/5.0")
                                .desired_width(400.0)
                                .desired_rows(3),
                        );
                        ui.add_space(16.0);
                        let mut play = false;
                        let mut cancel = false;
                        ui.horizontal(|ui| {
                            let can_play = !self.stream_url.trim().is_empty();
                            if ui.add_enabled(can_play, egui::Button::new("播放")).clicked() {
                                play = true;
                            }
                            ui.add_space(8.0);
                            if ui.button("取消").clicked() {
                                cancel = true;
                            }
                        });
                        (play, cancel)
                    })
                    .inner
            });
        let (play_clicked, cancel_clicked) = dialog_resp.inner;
        if play_clicked {
            let url = self.stream_url.trim().to_string();
            let headers = {
                let h = self.stream_headers.trim();
                if h.is_empty() {
                    None
                } else {
                    Some(h.to_string())
                }
            };
            self.stream_dialog_open = false;
            self.stream_url.clear();
            self.stream_headers.clear();
            self.start_stream(url, headers);
        } else if cancel_clicked || mask_resp.inner.clicked() {
            self.stream_dialog_open = false;
        }
    }

    /// 空状态 Ctrl+V 粘贴链接开播。
    fn handle_paste_stream(&mut self, ctx: &egui::Context) {
        if self.stream_dialog_open || self.player.is_some() {
            return;
        }
        let mut pasted: Option<String> = None;
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                if let egui::Event::Paste(s) = e {
                    pasted = Some(s.clone());
                    return false;
                }
                true
            });
        });
        if let Some(text) = pasted {
            let url = text.trim();
            if is_stream_url(url) {
                self.start_stream(url.to_string(), None);
            }
        }
    }

    /// 串流播放：构造 PendingPlay（remote=None → mpv 直接 loadfile URL）。
    fn start_stream(&mut self, url: String, headers: Option<String>) {
        let pending = player_app::PendingPlay {
            source_id: 0,
            items: vec![player_app::MediaItem {
                name: url.clone(),
                path: std::path::PathBuf::from(&url),
                size: 0,
                subs: vec![],
            }],
            start_index: 0,
            remote: None,
            http_headers: headers,
        };
        let ctx = self.egui_ctx.clone();
        self.start_play(&ctx, pending);
    }

}

/// 串流 URL 协议校验。
fn is_stream_url(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "http://",
        "https://",
        "rtmp://",
        "rtsp://",
        "mms://",
        "mmsh://",
        "ftp://",
        "ftps://",
    ];
    PREFIXES.iter().any(|p| s.starts_with(p))
}


fn query_sources(conn: &rusqlite::Connection) -> anyhow::Result<Vec<MediaSource>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, name, endpoint, username, root, extra_json \
         FROM sources ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        let parsed = (|| -> rusqlite::Result<Option<MediaSource>> {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let name: String = row.get(2)?;
            let endpoint: String = row.get(3)?;
            let username: String = row.get(4)?;
            let root: String = row.get(5)?;
            let extra_json: String = row.get(6)?;
            let Some(kind) = SourceKind::from_db(&kind) else {
                tracing::warn!("跳过未知类型的媒体源记录: kind={kind}");
                return Ok(None);
            };
            let accept_invalid_certs = serde_json::from_str::<serde_json::Value>(&extra_json)
                .ok()
                .and_then(|v| v.get("accept_invalid_certs").and_then(|b| b.as_bool()))
                .unwrap_or(false);
            // 本地源的路径存于 endpoint（root 列为空）；远程源 root 为起始子路径。
            let root = if root.is_empty() && kind == SourceKind::Local {
                PathBuf::from(&endpoint)
            } else {
                PathBuf::from(&root)
            };
            Ok(Some(MediaSource {
                id,
                name,
                kind,
                endpoint,
                username,
                root,
                accept_invalid_certs,
            }))
        })();
        if let Err(e) = &parsed {
            tracing::warn!("跳过无法解码的媒体源记录: {e}");
        }
        Ok(parsed)
    })?;
    Ok(rows
        .filter_map(|r| match r {
            Ok(Ok(opt)) => opt,
            Ok(Err(e)) => {
                tracing::warn!("跳过无法解码的媒体源记录: {e}");
                None
            }
            Err(e) => {
                tracing::warn!("读取媒体源记录失败: {e}");
                None
            }
        })
        .collect())
}

fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/icons/dy-icon.png");
    let img = image::load_from_memory(bytes)
        .expect("failed to load dy-icon.png")
        .to_rgba8();
    let (w, h) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    }
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    let icon = load_icon();
    let options = eframe::NativeOptions {
        renderer: {
            #[cfg(feature = "backend-wgpu")]
            { eframe::Renderer::Wgpu }
            #[cfg(not(feature = "backend-wgpu"))]
            { eframe::Renderer::Glow }
        },
        viewport: {
            let vb = egui::ViewportBuilder::default()
                .with_title("DyPlayer")
                .with_icon(icon)
                // 隐藏创建：消除"建窗→初始化→首帧"期间的闪白/闪系统标题栏；
                // logic() 首帧装好样式后 Win32 直调显示（fs_patch::show_main_window）。
                .with_visible(false)
                .with_inner_size([1280.0, 760.0]);
            // wgpu：装饰窗口创建，首帧 prime_borderless 切 THICKFRAME 无边框
            // （with_decorations(false) 在 Win10 虚拟机会全黑，勿回退）。
            // glow：with_decorations(false) 直接创建无装饰（winit 几何一致；
            // prime_borderless 的 WM_NCCALCSIZE 在 glow 后端下致 winit 几何不一致
            // → 标题栏无高度 + 鼠标坐标偏移）。
            #[cfg(not(feature = "backend-wgpu"))]
            { vb.with_decorations(false) }
            #[cfg(feature = "backend-wgpu")]
            { vb }
        },
        ..Default::default()
    };
    eframe::run_native(
        "DyPlayer",
        options,
        Box::new(move |cc| {
            let app = DyPlayerApp::new(cc)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

/// debug：日志打到控制台（cargo run 可见）；release：GUI 子系统无控制台，追加写到
/// `%LOCALAPPDATA%\dyplayer\dyplayer.log`。
/// 冻结 watchdog：frame_seq 停滞超 3 秒且 phase 非 idle（player 视图有 100ms 保底
/// repaint，停滞即卡死；浏览视图空闲时不持续 repaint，phase=idle 需跳过防误报）时
/// 触发取证——记录实时 phase 与 mpv 状态，并写全线程 minidump 供事后分析。
fn spawn_freeze_watchdog(probe: Arc<FreezeProbe>) {
    std::thread::Builder::new()
        .name("freeze-watchdog".into())
        .spawn(move || {
            let mut last_seq = 0u64;
            let mut last_change = std::time::Instant::now();
            let mut reported = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let seq = probe.frame_seq.load(std::sync::atomic::Ordering::Relaxed);
                if seq != last_seq {
                    last_seq = seq;
                    last_change = std::time::Instant::now();
                    reported = false;
                    continue;
                }
                if !reported && seq > 0 && last_change.elapsed().as_secs() >= 3 {
                    let phase = probe.phase.load(std::sync::atomic::Ordering::Relaxed);
                    if phase == PH_IDLE {
                        continue;
                    }
                    reported = true;
                    tracing::error!(
                        "FREEZE DETECTED: frame_seq stalled at {seq}, phase={} ({phase}), dumping...",
                        phase_name(phase)
                    );
                    write_freeze_dump();
                }
            }
        })
        .expect("spawn freeze watchdog");
}

/// 写当前进程全线程 minidump 到数据目录 freeze.dmp（覆盖旧文件）。
#[cfg(windows)]
fn write_freeze_dump() {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Diagnostics::Debug::{
        MINIDUMP_TYPE, MiniDumpNormal, MiniDumpWithThreadInfo, MiniDumpWriteDump,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    let Some(dir) = dyplayer_store::AppConfig::data_dir().ok() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("freeze.dmp");
    let Ok(file) = std::fs::File::create(&path) else {
        tracing::error!("freeze dump: cannot create {:?}", path);
        return;
    };
    let dump_type = MINIDUMP_TYPE(MiniDumpNormal.0 | MiniDumpWithThreadInfo.0);
    let result = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            std::process::id(),
            HANDLE(file.as_raw_handle() as _),
            dump_type,
            None,
            None,
            None,
        )
    };
    match result {
        Ok(()) => tracing::error!("freeze dump written: {:?}", path),
        Err(e) => tracing::error!("freeze dump failed: {e}"),
    }
}

#[cfg(not(windows))]
fn write_freeze_dump() {}

fn init_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,epaint=error".into());
    if cfg!(debug_assertions) {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
        return;
    }
    // release：logs/ 子目录按天轮转，保留 7 天；关 ANSI 颜色码（文件无需着色）。
    let path = dyplayer_store::AppConfig::data_dir()
        .map(|dir| dir.join("logs"))
        .ok()
        .and_then(|logs_dir| {
            std::fs::create_dir_all(&logs_dir).ok()?;
            cleanup_old_logs(&logs_dir, 7);
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            Some(logs_dir.join(format!("dyplayer-{date}.log")))
        });
    match path
        .and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok()
                .map(|f| (p, f))
        }) {
        Some((path, file)) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            tracing::info!("--- dyplayer 启动（日志: {}） ---", path.display());
        }
        // 日志文件也开不出来（如数据目录不可写）：丢弃日志，不阻塞启动。
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_ansi(false)
                .with_writer(std::io::sink)
                .try_init();
        }
    }
}

/// 删除 logs_dir 下 mtime 早于 keep_days 天前的 .log 文件（按天轮转清理）。
fn cleanup_old_logs(logs_dir: &std::path::Path, keep_days: u32) {
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(60 * 60 * 24 * keep_days as u64))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|mtime| mtime < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_sources_skips_invalid_utf8_rows() {
        let store = dyplayer_store::Store::open(&PathBuf::from(":memory:")).unwrap();
        let sources = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', cast(x'4142FF' AS TEXT), 'D:\\bad')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', 'good', 'D:\\good')",
                [],
            )
            .unwrap();
            query_sources(conn).unwrap()
        });
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "good");
        assert_eq!(sources[0].kind, SourceKind::Local);
        assert_eq!(sources[0].root, PathBuf::from("D:\\good"));
    }

    #[test]
    fn query_sources_parses_remote_kinds_and_extra() {
        let store = dyplayer_store::Store::open(&PathBuf::from(":memory:")).unwrap();
        let sources = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint, username, extra_json) \
                 VALUES('webdav', 'nas', 'https://nas.local/dav', 'u', '{\"accept_invalid_certs\":true}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('smb', 'share', 'smb://nas/media')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('ftp', 'legacy', 'ftp://x')",
                [],
            )
            .unwrap();
            query_sources(conn).unwrap()
        });
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].kind, SourceKind::WebDav);
        assert!(sources[0].accept_invalid_certs);
        assert_eq!(sources[1].kind, SourceKind::Smb);
        // 未知 kind（ftp）被跳过，不进入列表
    }

    #[test]
    fn cleanup_old_logs_removes_stale_and_keeps_recent() {
        let dir = std::env::temp_dir()
            .join(format!("dyplayer_log_test_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let recent = dir.join("dyplayer-recent.log");
        std::fs::write(&recent, b"recent").unwrap();

        let stale = dir.join("dyplayer-stale.log");
        std::fs::write(&stale, b"stale").unwrap();
        let old_time = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(60 * 60 * 24 * 8))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();

        let other = dir.join("README.txt");
        std::fs::write(&other, b"keep").unwrap();

        cleanup_old_logs(&dir, 7);

        assert!(recent.exists(), "近期 .log 应保留");
        assert!(!stale.exists(), "8 天前 .log 应删除");
        assert!(other.exists(), "非 .log 文件应保留");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cleanup_old_logs_handles_missing_dir() {
        let dir = std::env::temp_dir().join("dyplayer_log_test_nonexistent_456789");
        std::fs::remove_dir_all(&dir).ok();
        cleanup_old_logs(&dir, 7);
    }
}
