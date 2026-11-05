//! 弹幕 overlay（M2 对接层）：session/atlas/纹理生命周期 + 帧渲染 + 设置面板。
//!
//! 契约（TDD 6.5）：
//! - file-loaded 自动加载同名 `.xml`（无文件/无字体则弹幕静默关闭）
//! - 渲染入口只接收 MediaClock 的媒体时间（单源时钟），绘制于视频之上、UI 之下
//! - viewport 跟随播放区域（中央面板 avail × pixels_per_point），
//!   尺寸变化经 set_params 异步重 prepare（无闪烁）
//! - 透明度/延迟即时生效；字号/显示区域/速度经 set_params 重 prepare

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use dyplayer_danmaku::dfm::FrameItem;
use dyplayer_danmaku::dfm_core::model::DanmakuType;
use dyplayer_danmaku::layout::{DanmakuFont, LayoutParams};
use dyplayer_danmaku::normalize::{NormalizedItem, parse_danmaku_xml};
use dyplayer_danmaku::render::{GlyphAtlas, MeshParams, build_danmaku_mesh};
use dyplayer_danmaku::session::DanmakuSession;

use crate::glyph::{fade, mix};
use crate::player_app::osc::{OscTheme, osc_font};

/// R12-B1：后台 XML 读取+解析结果（on_file_loaded 异步化）。
struct XmlLoadResult {
    xml_path: PathBuf,
    items: Arc<Vec<NormalizedItem>>,
}

/// 视频同名弹幕文件路径（`a.mp4` -> `a.xml`）。
pub fn xml_path_for_video(video_path: &Path) -> PathBuf {
    video_path.with_extension("xml")
}


pub struct DanmakuOverlay {
    pub enabled: bool,
    /// 全局不透明度（0.1 - 1.0，即时生效）。
    pub opacity: f32,
    /// 延迟秒（正=弹幕延后，即时生效：查询侧时间平移）。
    pub delay_seconds: f64,
    pub settings_open: bool,
    font_size_cfg: f32,
    display_area_cfg: f64,
    speed_cfg: f64,
    /// dy 扩展渐变开关（默认关；渲染参数即时生效，不重排布局）。
    /// pub(crate)：M2 覆盖窗口线程经 OverlayRunState 每帧同步。
    pub(crate) gradient_cfg: bool,
    /// 渐变端点对称收缩量（0-0.25；即时项：渲染参数本帧生效）。
    pub(crate) gradient_inset_cfg: f64,
    /// 顶部弹幕（FixTop）显示开关（即时项：查询侧过滤本帧生效，
    /// 不重排布局）。pub(crate)：M2 覆盖窗口线程经 OverlayRunState 同步。
    pub show_top_cfg: bool,
    /// 底部弹幕（FixBottom）显示开关（即时项，同上）。
    pub show_bottom_cfg: bool,
    /// 描边宽度 profile（0-4；布局项——宽度参与碰撞测量，异步重 prepare）。
    outline_cfg: f64,
    /// 弹幕字体选择（默认 / 包内 HarmonyOS / 自定义字体文件）。
    font_choice_cfg: dyplayer_store::DanmakuFontChoice,
    /// 会话内记忆的自定义字体路径（rfd 选择成功后更新；切默认/应用自带
    /// 不清空，供"自定义"pill 切回；启动时从 config.font == Custom 恢复）。
    pub(crate) custom_font_path: Option<PathBuf>,
    /// 设置窗口"自定义…"待处理的字体文件选择请求（帧末弹出 rfd 对话框）。
    pub(crate) pending_font_pick: bool,
    /// 弹幕文件/字体选择推迟到宿主线程（GPU 覆盖层线程不能弹 rfd）：
    /// true 时 pending_* 置位后**不在本线程消费**，由覆盖线程取走转
    /// OscCmd 命令 → 主线程 rfd → 结果经桥回传消费。SW 主线程 false
    /// 走原同步 rfd 路径。
    pub(crate) defer_file_dialogs: bool,
    /// 最近一次自定义字体选择结果（成功=字体名 / 失败=原因），显示于设置窗口。
    pub(crate) last_font_result: Option<Result<String, String>>,
    session: Option<DanmakuSession>,
    atlas: GlyphAtlas,
    tex: Option<egui::TextureHandle>,
    /// 上次同步的 viewport 物理尺寸（量化 key，变化才 set_params）。
    viewport_key: (u32, u32),
    font_unavailable: bool,
    /// 设置窗口“载入弹幕文件…”待处理请求（帧末弹出 rfd 对话框）。
    pub(crate) pending_pick: bool,
    /// 弹窗内手动选择并加载成功的弹幕文件（待 player_app 落绑定关系，
    /// 消费后清空；换集作废）。
    pub manual_pick: Option<PathBuf>,
    /// 最近一次手动载入结果（成功条数/失败原因），显示于设置窗口。
    pub(crate) last_load_result: Option<Result<usize, String>>,
    /// 诊断：上一帧可见弹幕数。
    pub last_visible: usize,
    /// R12-B1：后台 XML 读取+解析任务（on_file_loaded 异步化）。
    pending_xml_load: Option<mpsc::Receiver<XmlLoadResult>>,
}

impl Default for DanmakuOverlay {
    fn default() -> Self {
        Self::new()
    }
}

/// 弹幕参数默认值（还原按钮 / new() 共用，改这里即可全局对齐）。
/// 字体默认值 = DanmakuFontChoice::Default（带 String 的枚举不能做 const，
/// 内联于 new() / export 处）。
const DEFAULT_OPACITY: f32 = 1.0;
const DEFAULT_DELAY: f64 = 0.0;
const DEFAULT_FONT_SIZE: f32 = 30.0;
const DEFAULT_AREA: f64 = 1.0;
const DEFAULT_SPEED: f64 = 1.0;
const DEFAULT_GRADIENT: bool = false;
const DEFAULT_GRADIENT_INSET: f64 = 0.0;
const DEFAULT_OUTLINE: f64 = 2.0;
const DEFAULT_SHOW_TOP: bool = true;
const DEFAULT_SHOW_BOTTOM: bool = true;

impl DanmakuOverlay {
    pub fn new() -> Self {
        Self {
            enabled: true,
            opacity: DEFAULT_OPACITY,
            delay_seconds: DEFAULT_DELAY,
            settings_open: false,
            font_size_cfg: DEFAULT_FONT_SIZE,
            display_area_cfg: DEFAULT_AREA,
            speed_cfg: DEFAULT_SPEED,
            gradient_cfg: DEFAULT_GRADIENT,
            gradient_inset_cfg: DEFAULT_GRADIENT_INSET,
            show_top_cfg: DEFAULT_SHOW_TOP,
            show_bottom_cfg: DEFAULT_SHOW_BOTTOM,
            outline_cfg: DEFAULT_OUTLINE,
            font_choice_cfg: dyplayer_store::DanmakuFontChoice::Default,
            custom_font_path: None,
            pending_font_pick: false,
            defer_file_dialogs: false,
            last_font_result: None,
            session: None,
            atlas: GlyphAtlas::new(),
            tex: None,
            viewport_key: (0, 0),
            font_unavailable: false,
            pending_pick: false,
            manual_pick: None,
            last_load_result: None,
            last_visible: 0,
            pending_xml_load: None,
        }
    }

    /// 启动时应用持久化的全局弹幕设置（config.toml [danmaku] 节；
    /// 须在首个 session 建立前调用——单次启动恢复，非热更新入口）。
    pub fn apply_config(&mut self, cfg: &dyplayer_store::DanmakuConfig) {
        self.enabled = cfg.enabled;
        self.opacity = cfg.opacity;
        self.font_size_cfg = cfg.font_size;
        self.display_area_cfg = f64::from(cfg.display_area);
        self.speed_cfg = f64::from(cfg.speed);
        self.gradient_cfg = cfg.gradient;
        self.gradient_inset_cfg = f64::from(cfg.gradient_inset);
        self.show_top_cfg = cfg.show_top;
        self.show_bottom_cfg = cfg.show_bottom;
        self.outline_cfg = f64::from(cfg.outline_width);
        self.font_choice_cfg = cfg.font.clone();
        if let dyplayer_store::DanmakuFontChoice::Custom(path) = &self.font_choice_cfg {
            self.custom_font_path = Some(PathBuf::from(path));
        }
    }

    /// 导出当前全局弹幕设置（持久化时调用）。
    pub fn export_config(&self) -> dyplayer_store::DanmakuConfig {
        dyplayer_store::DanmakuConfig {
            enabled: self.enabled,
            opacity: self.opacity,
            font_size: self.font_size_cfg,
            display_area: self.display_area_cfg as f32,
            speed: self.speed_cfg as f32,
            gradient: self.gradient_cfg,
            gradient_inset: self.gradient_inset_cfg as f32,
            show_top: self.show_top_cfg,
            show_bottom: self.show_bottom_cfg,
            outline_width: self.outline_cfg as f32,
            font: self.font_choice_cfg.clone(),
        }
    }

    pub fn item_count(&self) -> usize {
        self.session.as_ref().map(|s| s.item_count()).unwrap_or(0)
    }

    #[cfg(test)]
    fn session_is_none_for_test(&self) -> bool {
        self.session.is_none()
    }

    /// 换集：丢弃旧轨道（file-loaded 时按新文件重建）。字体选择与
    /// custom_font_path 为全局/会话级状态，不随换集清理。
    pub fn on_media_change(&mut self) {
        self.session = None;
        // R13：清 session 须同时清图集——旧字形残留会命中新 session 的查询。
        self.atlas = GlyphAtlas::new();
        self.tex = None;
        self.pending_pick = false;
        self.manual_pick = None;
        self.last_load_result = None;
        self.pending_font_pick = false;
        self.last_font_result = None;
        // R12-B1：丢弃后台 XML 加载结果（换集作废）。
        self.pending_xml_load = None;
    }

    /// file-loaded：加载弹幕 XML。`preferred` 为绑定表记住的弹幕文件
    /// （上次手动指定/同名加载），文件缺失时回退同名 `.xml`；无文件则无弹幕。
    /// 返回实际加载的弹幕文件路径（供调用方查询/更新该 (视频, 弹幕) 对的延迟）。
    /// R12-B1：文件读取+XML 解析移到后台线程，避免大 XML/慢盘阻塞 UI。
    pub fn on_file_loaded(
        &mut self,
        video_path: &Path,
        preferred: Option<&Path>,
    ) -> Option<PathBuf> {
        let path = preferred
            .map(|p| p.to_path_buf())
            .filter(|p| p.exists())
            .or_else(|| {
                let same = xml_path_for_video(video_path);
                same.exists().then_some(same)
            });
        let Some(path) = path else {
            tracing::info!("DANMAKU: no xml for {:?}", video_path.file_name());
            self.session = None;
            return None;
        };
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        std::thread::Builder::new()
            .name("danmaku-xml".into())
            .spawn(move || {
                let items = std::fs::read_to_string(&p)
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .map(|xml| Arc::new(parse_danmaku_xml(&xml)))
                    .unwrap_or_else(|| Arc::new(Vec::new()));
                let _ = tx.send(XmlLoadResult { xml_path: p, items });
            })
            .ok();
        self.pending_xml_load = Some(rx);
        Some(path)
    }

    /// R12-B1：收取后台 XML 加载结果（每帧调用）。
    pub fn poll_xml_load(&mut self) {
        let Some(rx) = &self.pending_xml_load else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending_xml_load = None;
        if result.items.is_empty() {
            tracing::info!("DANMAKU: no items from {:?}", result.xml_path.file_name());
            self.session = None;
            return;
        }
        self.ensure_session();
        if let Some(sess) = &mut self.session {
            sess.load_items(result.items);
            tracing::info!(
                "DANMAKU: loaded {} items from {:?}",
                sess.item_count(),
                result.xml_path.file_name()
            );
        } else {
            tracing::info!("DANMAKU: xml found but session unavailable (font?)");
        }
    }

    /// 测试辅助：阻塞等待后台 XML 加载完成并消费（on_file_loaded 异步化后
    /// session 在 poll_xml_load 时才创建，测试需显式 flush）。
    #[cfg(test)]
    pub(crate) fn flush_pending_xml_load_for_test(&mut self) {
        for _ in 0..500 {
            self.poll_xml_load();
            if self.pending_xml_load.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }


    /// 手动载入指定 XML 弹幕文件（热替换当前弹幕轨，generation+1 无闪烁）。
    /// 返回成功条数或用户可读的失败原因。
    pub fn load_xml_file(&mut self, path: &Path) -> Result<usize, String> {
        let xml = std::fs::read_to_string(path).map_err(|e| format!("读取弹幕文件失败: {e}"))?;
        self.load_xml_content(&xml)
    }

    /// 从 XML 文本内容加载弹幕（远程源下载后调用；本地文件入口为 `load_xml_file`）。
    pub fn load_xml_content(&mut self, xml: &str) -> Result<usize, String> {
        if xml.trim().is_empty() {
            return Err("弹幕文件为空".into());
        }
        self.ensure_session();
        let sess = self
            .session
            .as_mut()
            .ok_or_else(|| "弹幕不可用：未找到系统字体".to_string())?;
        sess.load_xml(xml);
        let count = sess.item_count();
        if count == 0 {
            Err("弹幕文件中没有可用弹幕".into())
        } else {
            Ok(count)
        }
    }

    fn ensure_session(&mut self) {
        if self.session.is_some() || self.font_unavailable {
            return;
        }
        match DanmakuFont::from_paths(&font_candidates(&self.font_choice_cfg)) {
            Some(font) => {
                let mut params = LayoutParams {
                    base_font_size: self.font_size_cfg,
                    display_area: self.display_area_cfg,
                    scroll_speed_factor: self.speed_cfg,
                    outline_profile: self.outline_cfg,
                    ..LayoutParams::default()
                };
                if self.viewport_key.0 > 0 {
                    params.viewport_width = f64::from(self.viewport_key.0);
                    params.viewport_height = f64::from(self.viewport_key.1);
                }
                self.session = Some(DanmakuSession::new(font, params));
            }
            None => {
                self.font_unavailable = true;
                tracing::warn!("danmaku unavailable: no system font in candidate chain");
            }
        }
    }

    /// 帧渲染（CentralPanel 内、视频 image 之后调用）。
    /// `viewport_rect` 为弹幕铺满的区域（播放区域，非视频矩形）。
    pub fn render(&mut self, ui: &mut egui::Ui, viewport_rect: egui::Rect, media_time: f64) {
        if !self.enabled {
            // 清零 last_visible：ui() 末尾据此决定是否请求满帧率重绘，
            // 禁用后残留旧值会导致无谓的连续 repaint。
            self.last_visible = 0;
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        self.sync_viewport(viewport_rect, ppp);
        let Some(sess) = &mut self.session else {
            self.last_visible = 0;
            return;
        };
        sess.poll();
        sess.set_offset_seconds(self.delay_seconds);

        let font = Arc::clone(sess.layout_font());
        let outline_profile = sess.params().outline_profile;
        // 顶部/底部开关（即时项）：查询侧过滤本帧生效，不触发重排。
        let (show_top, show_bottom) = (self.show_top_cfg, self.show_bottom_cfg);
        let mut visible: Vec<(FrameItem, _)> = Vec::new();
        sess.for_each_visible_item(media_time, |frame, item| {
            if !item.is_scroll && !fixed_type_visible(item.type_code, show_top, show_bottom) {
                return;
            }
            visible.push((frame, item));
        });
        if visible.is_empty() {
            self.last_visible = 0;
            return;
        }
        self.last_visible = visible.len();
        // 先保证 handle 存在（首次创建空纹理；后续帧 not dirty 时 no-op），
        // 拿到 texture_id 供 build 使用。build 期间 prime_atlas 可能触发
        // grow/reset + dirty，故 build 后再调一次 ensure_texture 上传含新
        // 字形的快照——避免 mesh UV（按新尺寸/坐标）与纹理（旧尺寸旧内容）
        // 错位的首帧色块，并确保 grow 后新尺寸纹理同帧就位（AGENTS.md 既有
        // 坑的同族：ensure_texture 早于 prime 是色块 + 翻倍长帧的共同根因）。
        let Some(texture_id) = Self::ensure_texture(&mut self.atlas, &mut self.tex, ui.ctx())
        else {
            return;
        };
        let params = MeshParams {
            font: &font,
            outline_profile,
            video_rect: viewport_rect,
            pixels_per_point: ppp,
            global_opacity: self.opacity,
            texture_id,
            gradient_enabled: self.gradient_cfg,
            gradient_inset: self.gradient_inset_cfg as f32,
        };
        let mesh = build_danmaku_mesh(&mut self.atlas, &params, &visible);
        Self::ensure_texture(&mut self.atlas, &mut self.tex, ui.ctx());
        ui.painter().add(egui::Shape::Mesh(Arc::new(mesh)));
    }

    fn sync_viewport(&mut self, video_rect: egui::Rect, ppp: f32) {
        let w = (video_rect.width() * ppp).round();
        let h = (video_rect.height() * ppp).round();
        if !(w >= 1.0 && h >= 1.0) {
            return;
        }
        let key = (w as u32, h as u32);
        if self.viewport_key == key {
            return;
        }
        self.viewport_key = key;
        if let Some(sess) = &mut self.session {
            let mut params = sess.params().clone();
            params.viewport_width = f64::from(key.0);
            params.viewport_height = f64::from(key.1);
            sess.set_params(params);
        }
    }

    fn ensure_texture(
        atlas: &mut GlyphAtlas,
        tex: &mut Option<egui::TextureHandle>,
        ctx: &egui::Context,
    ) -> Option<egui::TextureId> {
        if atlas.dirty() {
            let image = atlas.image();
            match tex {
                Some(handle) => handle.set(image, egui::TextureOptions::LINEAR),
                None => {
                    *tex = Some(ctx.load_texture(
                        "danmaku-glyph-atlas",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            atlas.clear_dirty();
        }
        tex.as_ref().map(|handle| handle.id())
    }

    fn apply_params(&mut self) {
        if let Some(sess) = &mut self.session {
            let mut params = sess.params().clone();
            params.base_font_size = self.font_size_cfg;
            params.display_area = self.display_area_cfg;
            params.scroll_speed_factor = self.speed_cfg;
            params.outline_profile = self.outline_cfg;
            sess.set_params(params);
        }
    }

    /// 字体选择热切换（设置面板调用）：成功加载新字体则热替换 session
    /// （generation+1 异步重排，沿用旧 layout 无闪烁）；atlas 缓存 key 无
    /// 字体维度，须整体重建（纹理随 dirty 重新上传）。加载失败保持当前
    /// 字体（warn；选择已记录，下次 ensure_session / 重启重试）。
    pub fn set_font_choice(&mut self, choice: dyplayer_store::DanmakuFontChoice) {
        self.font_choice_cfg = choice;
        if self.session.is_none() {
            // session 未建立：重置不可用标记，下次 file-loaded 按新选择重试。
            // R13：无 session 时换字体也须清图集——旧字形会命中新 session。
            self.font_unavailable = false;
            self.atlas = GlyphAtlas::new();
            self.tex = None;
            return;
        }
        let Some(font) = DanmakuFont::from_paths(&font_candidates(&self.font_choice_cfg)) else {
            tracing::warn!(
                "danmaku font {:?} unavailable, keep current font",
                self.font_choice_cfg
            );
            return;
        };
        if let Some(sess) = &mut self.session {
            sess.set_font(font);
        }
        self.atlas = GlyphAtlas::new();
        self.tex = None;
    }

    /// 弹幕设置弹窗（ui() 顶层调用；即时项本帧生效，布局项异步重 prepare）。
    /// 定制深色半透明卡片（媒体源弹窗 ui_edit_modal 同族工艺）：独立
    /// Foreground 层绘制（egui hit test 只跨层遮挡，弹窗打开期间下层画面
    /// 与 OSC 不抢事件），锚定"弹幕"按钮上方；点击外部 / ESC / ✕ 关闭。
    /// `anchor`：卡片底边中心锚点（"弹幕"按钮中轴线、按钮栏顶部之上）；
    /// None（按钮 rect 尚未记录的首帧）时回退右下角。
    pub fn show_settings(
        &mut self,
        ui: &mut egui::Ui,
        anchor: Option<egui::Pos2>,
        theme: OscTheme,
    ) {
        if !self.settings_open {
            return;
        }
        let screen = ui.max_rect();
        let layer =
            egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("danmaku-settings"));
        let mut lui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer)
                .max_rect(screen)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );

        // 全屏透明命中层：点击外部（主/右键）或 ESC 关闭。不加压暗遮罩——
        // 弹幕设置是观影中的轻量弹层（与 OSC 浮层同族），前景层本身已拦截
        // 下层指针，打开期间画面点击与 OSC 按钮不会误触。
        let outside =
            lui.interact(screen, egui::Id::unique("danmaku-settings-outside"), egui::Sense::click());
        let close_now = outside.clicked()
            || outside.secondary_clicked()
            || lui.ctx().input(|i| i.key_pressed(egui::Key::Escape));

        // 固定尺寸卡片：内容行高全固定，结果反馈行按需增高（弹幕文件
        // 载入结果与字体选择结果各一行）。字体选择行 +33（26 行高 + 7 间距）、
        // 描边滑条 +33、渐变 check +40（7 + 26 + 7）。
        let result_extra = if self.last_load_result.is_some() { 22.0 } else { 0.0 }
            + if self.last_font_result.is_some() { 22.0 } else { 0.0 };
        let size = egui::vec2(300.0, 486.0 + result_extra);
        let card = settings_card_rect(screen, anchor, size);
        let p = lui.painter();
        p.rect_filled(card, egui::CornerRadius::same(14), settings_card_bg());
        p.rect_stroke(
            card.shrink(0.5),
            egui::CornerRadius::same(14),
            egui::Stroke::new(1.0, settings_card_outline()),
            egui::StrokeKind::Inside,
        );
        // 卡片命中层：吸收点击，防止点进卡片内部触发 outside 误关。
        let _ =
            lui.interact(card, egui::Id::unique("danmaku-settings-card"), egui::Sense::click());

        let mut mui = lui.new_child(
            egui::UiBuilder::new()
                .id_salt("danmaku-settings-body")
                .max_rect(card.shrink2(egui::vec2(22.0, 18.0)))
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        mui.spacing_mut().item_spacing.y = 0.0;

        // 标题行 + 关闭 ✕（hover 圆底 + 自绘交叉线，无字形依赖）。
        let mut close_x = false;
        {
            let (rect, _) = mui.allocate_exact_size(
                egui::vec2(mui.available_width(), 26.0),
                egui::Sense::hover(),
            );
            let p = mui.painter();
            p.text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                "弹幕设置",
                osc_font(15.0),
                theme.text,
            );
            // ✕ 完整落在内容区内（中心贴右界会溢出 12px，右侧边距视觉变窄）。
            let x_rect = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 24.0, rect.center().y - 12.0),
                egui::vec2(24.0, 24.0),
            );
            let resp = mui
                .interact(x_rect, egui::Id::unique("danmaku-settings-close"), egui::Sense::click());
            let t = mui.ctx().animate_value_with_time(
                egui::Id::unique("danmaku-settings-close-h"),
                resp.hovered() as u8 as f32,
                0.08,
            );
            if t > 0.01 {
                p.circle_filled(x_rect.center(), 11.0, fade(egui::Color32::WHITE, 0.10 * t));
            }
            let c = mix(theme.text_dim, theme.text, t);
            let k = 3.4 + 0.4 * t;
            p.line_segment(
                [
                    egui::pos2(x_rect.center().x - k, x_rect.center().y - k),
                    egui::pos2(x_rect.center().x + k, x_rect.center().y + k),
                ],
                egui::Stroke::new(1.4, c),
            );
            p.line_segment(
                [
                    egui::pos2(x_rect.center().x - k, x_rect.center().y + k),
                    egui::pos2(x_rect.center().x + k, x_rect.center().y - k),
                ],
                egui::Stroke::new(1.4, c),
            );
            if resp.clicked() {
                close_x = true;
            }
        }

        mui.add_space(6.0);
        draw_enabled_row(
            &mut mui,
            "dm-set-enabled",
            &mut self.enabled,
            &mut self.show_top_cfg,
            &mut self.show_bottom_cfg,
            theme,
        );
        mui.add_space(6.0);

        // 即时项：透明度 / 延迟（±10 分钟，0.5s 步进）。
        draw_slider_row(
            &mut mui,
            "dm-set-opacity",
            "透明度",
            0.1,
            1.0,
            DEFAULT_OPACITY,
            &mut self.opacity,
            &|v| format!("{:.0}%", v * 100.0),
            self.enabled,
            theme,
        );
        let mut delay = self.delay_seconds as f32;
        if draw_slider_row(
            &mut mui,
            "dm-set-delay",
            "延迟",
            -600.0,
            600.0,
            DEFAULT_DELAY as f32,
            &mut delay,
            &fmt_delay,
            self.enabled,
            theme,
        ) {
            delay = (delay / 0.5).round() * 0.5;
            self.delay_seconds = f64::from(delay);
        }

        // 布局项（变化后异步重 prepare）：字号（整数步进：避免拖动产生
        // 几十档 0.25px 字号的 glyph 堆积在 atlas 里耗尽容量）/ 显示区域 /
        // 滚动速度。
        let mut layout_changed = false;
        let mut fs = self.font_size_cfg;
        if draw_slider_row(
            &mut mui,
            "dm-set-fontsize",
            "字号",
            12.0,
            48.0,
            DEFAULT_FONT_SIZE,
            &mut fs,
            &|v| format!("{}", v.round() as i64),
            self.enabled,
            theme,
        ) {
            self.font_size_cfg = fs.round();
            layout_changed = true;
        }
        let mut area = self.display_area_cfg as f32;
        if draw_slider_row(
            &mut mui,
            "dm-set-area",
            "显示区域",
            0.25,
            1.0,
            DEFAULT_AREA as f32,
            &mut area,
            &|v| format!("{:.0}%", v * 100.0),
            self.enabled,
            theme,
        ) {
            self.display_area_cfg = f64::from(area);
            layout_changed = true;
        }
        let mut speed = self.speed_cfg as f32;
        if draw_slider_row(
            &mut mui,
            "dm-set-speed",
            "滚动速度",
            0.5,
            2.0,
            DEFAULT_SPEED as f32,
            &mut speed,
            &|v| format!("{v:.2}x"),
            self.enabled,
            theme,
        ) {
            self.speed_cfg = f64::from(speed);
            layout_changed = true;
        }
        // 描边宽度（0.25 档量化：atlas glyph key 含 radius 维度，连续拖动
        // 会堆几十档 radius 缓存；0 = 无描边）。
        let mut outline = self.outline_cfg as f32;
        if draw_slider_row(
            &mut mui,
            "dm-set-outline",
            "描边",
            0.0,
            4.0,
            DEFAULT_OUTLINE as f32,
            &mut outline,
            &|v| format!("{v:.2}"),
            self.enabled,
            theme,
        ) {
            outline = (outline / 0.25).round() * 0.25;
            self.outline_cfg = f64::from(outline);
            layout_changed = true;
        }
        if layout_changed {
            self.apply_params();
        }

        // dy 扩展渐变开关 + 渐变范围滑条（组合行，即时项：渲染参数本帧
        // 生效，不重排布局；关 = per-item 渐变降级默认配色，滑条禁用淡出）。
        mui.add_space(7.0);
        draw_gradient_row(
            &mut mui,
            "dm-set-gradient",
            &mut self.gradient_cfg,
            &mut self.gradient_inset_cfg,
            self.enabled,
            theme,
        );
        mui.add_space(7.0);

        // 字体选择（单击类：热替换 session 字体并异步重排）。
        // "自定义"pill 无记忆路径时点击=打开文件选择；有路径时点击=切换
        // 到该字体；已选中自定义时尾部叠修改按钮（另选文件）。
        match draw_font_choice_row(
            &mut mui,
            "dm-set-font",
            &self.font_choice_cfg,
            self.custom_font_path.as_deref(),
            self.enabled,
            theme,
        ) {
            Some(FontRowAction::SelectDefault) => {
                self.set_font_choice(dyplayer_store::DanmakuFontChoice::Default);
            }
            Some(FontRowAction::SelectApp) => {
                self.set_font_choice(dyplayer_store::DanmakuFontChoice::App);
            }
            Some(FontRowAction::SelectCustom) => {
                if let Some(path) = self.custom_font_path.clone() {
                    self.set_font_choice(dyplayer_store::DanmakuFontChoice::Custom(
                        path.to_string_lossy().into_owned(),
                    ));
                }
            }
            Some(FontRowAction::PickCustomFile) => {
                self.pending_font_pick = true;
            }
            None => {}
        }

        mui.add_space(7.0);
        let (hr, _) = mui.allocate_exact_size(
            egui::vec2(mui.available_width(), 1.0),
            egui::Sense::hover(),
        );
        mui.painter().rect_filled(
            hr,
            egui::CornerRadius::ZERO,
            fade(egui::Color32::WHITE, 0.10),
        );
        mui.add_space(7.0);

        // 底行：弹幕数 + 载入入口（描边胶囊，hover 转强调）。
        let mut pick_clicked = false;
        {
            let (rect, _) = mui.allocate_exact_size(
                egui::vec2(mui.available_width(), 30.0),
                egui::Sense::hover(),
            );
            let p = mui.painter();
            p.text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                format!("弹幕数 {}", self.item_count()),
                osc_font(12.0),
                fade(theme.text_dim, 0.85),
            );
            let gal = p.layout_no_wrap("载入弹幕文件…".to_owned(), osc_font(12.0), theme.text);
            let pw = gal.size().x + 22.0;
            let pill = egui::Rect::from_min_max(
                egui::pos2(rect.right() - pw, rect.center().y - 13.0),
                egui::pos2(rect.right(), rect.center().y + 13.0),
            );
            let resp =
                mui.interact(pill, egui::Id::unique("danmaku-settings-pick"), egui::Sense::click());
            let t = mui.ctx().animate_value_with_time(
                egui::Id::unique("danmaku-settings-pick-h"),
                resp.hovered() as u8 as f32,
                0.09,
            );
            p.rect_stroke(
                pill,
                egui::CornerRadius::same(13),
                egui::Stroke::new(1.0, mix(fade(egui::Color32::WHITE, 0.30), theme.accent, t)),
                egui::StrokeKind::Inside,
            );
            if t > 0.01 {
                p.rect_filled(pill, egui::CornerRadius::same(13), fade(theme.accent, 0.12 * t));
            }
            p.galley(
                egui::pos2(pill.left() + 11.0, pill.center().y - gal.size().y / 2.0),
                gal,
                theme.text,
            );
            if resp.clicked() {
                pick_clicked = true;
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }

        // 载入结果反馈（成功绿 / 失败红；换集清除）。
        if let Some(res) = &self.last_load_result {
            mui.add_space(4.0);
            let (rect, _) = mui.allocate_exact_size(
                egui::vec2(mui.available_width(), 18.0),
                egui::Sense::hover(),
            );
            let (txt, col) = match res {
                Ok(n) => (
                    format!("已载入 {n} 条弹幕"),
                    egui::Color32::from_rgb(0x7C, 0xD9, 0x92),
                ),
                Err(e) => (e.clone(), egui::Color32::from_rgb(0xE8, 0x7C, 0x7C)),
            };
            mui.painter().text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                txt,
                osc_font(12.0),
                col,
            );
        }

        // 字体选择结果反馈（成功=已应用字体名 / 失败=原因；换集清除）。
        if let Some(res) = &self.last_font_result {
            mui.add_space(4.0);
            let (rect, _) = mui.allocate_exact_size(
                egui::vec2(mui.available_width(), 18.0),
                egui::Sense::hover(),
            );
            let (txt, col) = match res {
                Ok(name) => (
                    format!("已应用字体 {name}"),
                    egui::Color32::from_rgb(0x7C, 0xD9, 0x92),
                ),
                Err(e) => (e.clone(), egui::Color32::from_rgb(0xE8, 0x7C, 0x7C)),
            };
            mui.painter().text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                txt,
                osc_font(12.0),
                col,
            );
        }

        if close_now || close_x {
            self.settings_open = false;
        }
        if pick_clicked {
            self.pending_pick = true;
        }
        // defer 模式（GPU 覆盖层线程）：pending_pick 保留待覆盖线程取走
        // 转命令（OscCmd::PickDanmakuFile），rfd 由主线程执行。
        if self.pending_pick && !self.defer_file_dialogs {
            self.pending_pick = false;
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择弹幕文件")
                .add_filter("弹幕文件 (*.xml)", &["xml"])
                .pick_file()
            {
                let res = self.load_xml_file(&path);
                if res.is_ok() {
                    // 加载成功才上报路径，供 player_app 绑定 (视频, 弹幕文件) 关系
                    self.manual_pick = Some(path);
                }
                self.last_load_result = Some(res);
            }
        }

        // 帧末处理"自定义…"字体选择请求（与弹幕文件选择同模式）：
        // rfd 选定后先经 from_paths 校验可加载，成功才落选择 + 热切换；
        // 失败保旧字体并反馈原因。
        // defer 模式：pending_font_pick 同理转命令（OscCmd::PickDanmakuFont）。
        if self.pending_font_pick && !self.defer_file_dialogs {
            self.pending_font_pick = false;
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择字体文件")
                .add_filter("字体文件", &["ttf", "otf", "ttc"])
                .pick_file()
            {
                if DanmakuFont::from_paths(std::slice::from_ref(&path)).is_some() {
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.custom_font_path = Some(path.clone());
                    self.set_font_choice(dyplayer_store::DanmakuFontChoice::Custom(
                        path.to_string_lossy().into_owned(),
                    ));
                    self.last_font_result = Some(Ok(name));
                } else {
                    self.last_font_result =
                        Some(Err("字体文件无法加载（仅支持 TTF/OTF/TTC）".into()));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 弹幕设置弹窗：定制绘制（媒体源弹窗 ui_edit_modal 同族工艺）
// ---------------------------------------------------------------------------

/// 弹窗卡片底色：黑上浮一层深灰（OSC 浮层同族质感），~95% 不透明微透视频。
/// pub(crate)：OSC 音轨/字幕菜单卡片同源复用（统一风格）。
pub(crate) fn settings_card_bg() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(0x1C, 0x1C, 0x22, 228)
}

/// 卡片描边：白色低透明（10%）。
pub(crate) fn settings_card_outline() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 26)
}

/// 弹窗卡片矩形：anchor 为底边中心（"弹幕"按钮上方锚定），None 回退屏幕
/// 右下角；水平/垂直屏内 clamp（按钮贴边时卡片不越界）。
/// pub(crate)：OSC 音轨/字幕菜单卡片同源复用（统一 clamp 行为）。
pub(crate) fn settings_card_rect(
    screen: egui::Rect,
    anchor: Option<egui::Pos2>,
    size: egui::Vec2,
) -> egui::Rect {
    let center = match anchor {
        Some(a) => egui::pos2(a.x, a.y - size.y / 2.0),
        None => egui::pos2(
            screen.right() - 24.0 - size.x / 2.0,
            screen.bottom() - 24.0 - size.y / 2.0,
        ),
    };
    let mut origin = egui::Rect::from_center_size(center, size).min;
    let min_x = screen.left() + 8.0;
    let max_x = (screen.right() - 8.0 - size.x).max(min_x);
    let min_y = screen.top() + 8.0;
    let max_y = (screen.bottom() - 8.0 - size.y).max(min_y);
    origin.x = origin.x.clamp(min_x, max_x);
    origin.y = origin.y.clamp(min_y, max_y);
    egui::Rect::from_min_size(origin, size)
}

/// 滑条指针横坐标 → 值（线性映射 + 双端 clamp；退化区间返回 min）。
fn slider_hit_value(x: f32, x0: f32, x1: f32, min: f32, max: f32) -> f32 {
    if x1 <= x0 || max <= min {
        return min;
    }
    let t = ((x - x0) / (x1 - x0)).clamp(0.0, 1.0);
    min + (max - min) * t
}

/// 渐变组合行（26px 单行省空间）：左侧"渐变"check（draw_check 同工艺
/// 方框 + label），右侧迷你滑条（0-0.25 对称收缩量，1% 量化）+ 右对齐
/// 百分比数值。即时项：拖动本帧生效不重排；渐变关闭时滑条区 35% 淡出
/// 不响应，check 保持可用（开渐变后滑条恢复）。
fn draw_gradient_row(
    ui: &mut egui::Ui,
    id: &str,
    checked: &mut bool,
    inset: &mut f64,
    enabled: bool,
    theme: OscTheme,
) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
    let p = ui.painter();
    // 弹幕开关关闭时整体 35% 淡出 + 交互禁用（与 draw_slider_row 的 enabled
    // 参数同语义）；渐变 check 关时仅滑条区淡出（active = enabled && checked）。
    let dim = if enabled { 1.0 } else { 0.35 };
    let active = enabled && *checked;

    // 左侧 check：方框 + 对勾 + "渐变" label（draw_check 视觉工艺）。
    // 命中区限定左段（方框 + label），不与右侧滑条区重叠。
    let box_r = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::vec2(16.0, 16.0),
    );
    let stroke0 = if *checked {
        theme.accent
    } else {
        fade(egui::Color32::WHITE, 0.30)
    };
    let stroke_c = fade(stroke0, dim);
    p.rect_stroke(
        box_r,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.3, stroke_c),
        egui::StrokeKind::Inside,
    );
    if *checked {
        p.rect_filled(box_r.shrink(2.0), egui::CornerRadius::same(3), fade(theme.accent, dim));
        let c = box_r.center();
        p.line_segment(
            [
                egui::pos2(c.x - 3.4, c.y + 0.4),
                egui::pos2(c.x - 1.2, c.y + 2.6),
            ],
            egui::Stroke::new(1.7, egui::Color32::WHITE),
        );
        p.line_segment(
            [
                egui::pos2(c.x - 1.2, c.y + 2.6),
                egui::pos2(c.x + 3.8, c.y - 2.6),
            ],
            egui::Stroke::new(1.7, egui::Color32::WHITE),
        );
    }
    p.text(
        egui::pos2(box_r.right() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "渐变",
        osc_font(12.5),
        fade(theme.text_dim, dim),
    );
    let label_right = box_r.right() + 8.0 + 26.0;
    let check_hit = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(label_right + 2.0, rect.bottom()),
    );
    let cresp = ui.interact(
        check_hit,
        egui::Id::unique(id).with("check"),
        egui::Sense::click(),
    );
    if enabled && cresp.clicked() {
        *checked = !*checked;
    }
    if enabled {
        cresp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }

    // 右侧迷你滑条（0-0.25 对称收缩量，1% 量化）+ 右对齐百分比数值。
    let val_c = if active {
        theme.text
    } else {
        fade(theme.text, 0.35)
    };
    let gal = p.layout_no_wrap(
        format!("{}%", (*inset * 100.0).round() as i64),
        osc_font(12.5),
        val_c,
    );
    let track_x0 = label_right + 8.0;
    let track_x1 = rect.right() - gal.size().x - 10.0;
    let cy = rect.center().y;
    let hit = egui::Rect::from_min_max(
        egui::pos2(track_x0, cy - 11.0),
        egui::pos2(track_x1.max(track_x0), cy + 11.0),
    );
    let sresp = ui.interact(
        hit,
        egui::Id::unique(id).with("inset"),
        egui::Sense::click_and_drag(),
    );
    if active
        && let Some(pt) = sresp.interact_pointer_pos()
    {
        let nv = slider_hit_value(pt.x, track_x0, track_x1, 0.0, 0.25);
        *inset = f64::from((nv / 0.01).round() * 0.01);
    }
    // 轨道 + 指针（OSC 音量条工艺；渐变关或弹幕关时整体 35% 淡出）。
    let alpha = if active { 1.0 } else { 0.35 };
    let frac = ((*inset / 0.25).clamp(0.0, 1.0)) as f32;
    let track_w = (track_x1 - track_x0).max(0.0);
    p.rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(track_x0, cy - 2.0),
            egui::vec2(track_w, 4.0),
        ),
        egui::CornerRadius::same(2),
        fade(egui::Color32::WHITE, 0.18 * alpha),
    );
    if active {
        p.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(track_x0, cy - 2.0),
                egui::vec2((track_w * frac).max(0.0), 4.0),
            ),
            egui::CornerRadius::same(2),
            fade(theme.accent, 0.85),
        );
        let knob_x = track_x0 + track_w * frac;
        p.circle_filled(egui::pos2(knob_x, cy), 5.0, theme.accent);
        p.circle_stroke(
            egui::pos2(knob_x, cy),
            5.0,
            egui::Stroke::new(1.2, fade(egui::Color32::WHITE, 0.65)),
        );
        sresp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    p.galley(
        egui::pos2(rect.right() - gal.size().x, cy - gal.size().y / 2.0),
        gal,
        val_c,
    );
}

/// 弹幕字体选择 → 候选路径链（依次尝试，任一命中即用）。
/// Default = 不指定字体，沿系统候选链（与旧版 from_system 行为一致）；
/// App = 包内 HarmonyOS Sans SC Medium（与 UI 主字体同源路径链）；
/// Custom = 用户自选字体文件。任一选择失败时交叉回退系统字体，保证弹幕可用。
fn font_candidates(choice: &dyplayer_store::DanmakuFontChoice) -> Vec<PathBuf> {
    let mut paths = match choice {
        dyplayer_store::DanmakuFontChoice::Default => vec![
            PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
            PathBuf::from(r"C:\Windows\Fonts\msyh.ttf"),
            PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
            PathBuf::from(r"C:\Windows\Fonts\simsun.ttc"),
        ],
        dyplayer_store::DanmakuFontChoice::App => crate::player_app::harmonyos_font_candidates(),
        dyplayer_store::DanmakuFontChoice::Custom(path) => vec![PathBuf::from(path)],
    };
    for fallback in [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ] {
        let p = PathBuf::from(fallback);
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths
}

/// check 方框 + 对勾自绘（原 draw_check / draw_gradient_row 同工艺）：
/// 方框描边色由调用方决定，勾选时 accent 填充 + 白对勾。
fn paint_check_box(
    p: &egui::Painter,
    box_r: egui::Rect,
    checked: bool,
    stroke_c: egui::Color32,
    accent: egui::Color32,
) {
    p.rect_stroke(
        box_r,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.3, stroke_c),
        egui::StrokeKind::Inside,
    );
    if !checked {
        return;
    }
    p.rect_filled(box_r.shrink(2.0), egui::CornerRadius::same(3), accent);
    let c = box_r.center();
    p.line_segment(
        [
            egui::pos2(c.x - 3.4, c.y + 0.4),
            egui::pos2(c.x - 1.2, c.y + 2.6),
        ],
        egui::Stroke::new(1.7, egui::Color32::WHITE),
    );
    p.line_segment(
        [
            egui::pos2(c.x - 1.2, c.y + 2.6),
            egui::pos2(c.x + 3.8, c.y - 2.6),
        ],
        egui::Stroke::new(1.7, egui::Color32::WHITE),
    );
}

/// 非滚动弹幕（FixTop/FixBottom）的查询侧可见性：顶部/底部开关即时项
/// （渲染层逐条过滤，不重排布局）。
fn fixed_type_visible(type_code: i32, show_top: bool, show_bottom: bool) -> bool {
    match type_code {
        c if c == DanmakuType::FixTop as i32 => show_top,
        c if c == DanmakuType::FixBottom as i32 => show_bottom,
        _ => true,
    }
}

/// 首行组合 check（26px 单行）：左侧"显示弹幕"主开关 + 右侧"顶部"/"底部"
/// 两个子开关（分别控制 FixTop/FixBottom 类弹幕显隐，查询侧过滤即时生效）。
/// 主开关关闭时右侧子开关 35% 淡出（视觉从属，与渐变行滑条淡出同语义）但
/// 保持可点（子状态独立记忆，主开关重开后恢复）。三命中区互不重叠。
fn draw_enabled_row(
    ui: &mut egui::Ui,
    id: &str,
    enabled: &mut bool,
    show_top: &mut bool,
    show_bottom: &mut bool,
    theme: OscTheme,
) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
    let p = ui.painter();
    let cy = rect.center().y;

    // 左侧主开关：方框 + "显示弹幕" label，命中区限定左段。
    let main_box = egui::Rect::from_center_size(egui::pos2(rect.left() + 8.0, cy), egui::vec2(16.0, 16.0));
    let main_stroke = if *enabled {
        theme.accent
    } else {
        fade(egui::Color32::WHITE, 0.30)
    };
    paint_check_box(p, main_box, *enabled, main_stroke, theme.accent);
    p.text(
        egui::pos2(main_box.right() + 8.0, cy),
        egui::Align2::LEFT_CENTER,
        "显示弹幕",
        osc_font(12.5),
        theme.text_dim,
    );
    let main_label_w = 4.0 * 12.5 + 2.0;
    let main_hit = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(main_box.right() + 8.0 + main_label_w, rect.bottom()),
    );
    let mresp = ui.interact(main_hit, egui::Id::unique(id), egui::Sense::click());
    if mresp.clicked() {
        *enabled = !*enabled;
    }
    mresp.on_hover_cursor(egui::CursorIcon::PointingHand);

    // 右侧两个子开关（右对齐）：[✓]顶部  [✓]底部。
    // 子项宽度 = box 16 + gap 6 + label 2 字（12.5px ≈ 25）。
    let sub_w = 16.0 + 6.0 + 27.0;
    let gap = 14.0;
    let sub_dim = if *enabled { 1.0 } else { 0.35 };
    let draw_sub = |checked: &mut bool, label: &str, right: f32, key: &str| {
        let box_r = egui::Rect::from_center_size(
            egui::pos2(right - sub_w + 8.0, cy),
            egui::vec2(16.0, 16.0),
        );
        let label_col = fade(theme.text_dim, sub_dim);
        let stroke_c = if *checked {
            fade(theme.accent, sub_dim)
        } else {
            fade(egui::Color32::WHITE, 0.30 * sub_dim)
        };
        paint_check_box(p, box_r, *checked, stroke_c, fade(theme.accent, sub_dim));
        p.text(
            egui::pos2(box_r.right() + 6.0, cy),
            egui::Align2::LEFT_CENTER,
            label,
            osc_font(12.5),
            label_col,
        );
        let hit = egui::Rect::from_min_max(
            egui::pos2(box_r.left() - 2.0, rect.top()),
            egui::pos2(right, rect.bottom()),
        );
        let resp = ui.interact(hit, egui::Id::unique(id).with(key), egui::Sense::click());
        if sub_dim > 0.5 && resp.clicked() {
            *checked = !*checked;
        }
        if sub_dim > 0.5 {
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    };
    draw_sub(show_bottom, "底部", rect.right(), "bottom");
    draw_sub(show_top, "顶部", rect.right() - sub_w - gap, "top");
}

/// 字体选择行的用户动作（draw_font_choice_row 返回，调用方分发）。
enum FontRowAction {
    SelectDefault,
    SelectApp,
    SelectCustom,
    PickCustomFile,
}

/// 文本按最大字符数截断（超出补省略号；CJK 与拉丁统一按字符计，
/// 弹窗 pill 宽度预算下的近似够用）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}…")
}

/// 画 pill 底（选中高亮/hover 过渡/描边/命中响应），文本由调用方画。
fn draw_pill_bg(
    ui: &egui::Ui,
    p: &egui::Painter,
    pill: egui::Rect,
    id: egui::Id,
    selected: bool,
    enabled: bool,
    theme: OscTheme,
) -> egui::Response {
    let mut resp = ui.interact(pill, id, egui::Sense::click());
    let t = ui.ctx().animate_value_with_time(
        id.with("h"),
        (enabled && resp.hovered()) as u8 as f32,
        0.08,
    );
    let alpha = if enabled { 1.0 } else { 0.35 };
    if selected {
        p.rect_filled(pill, egui::CornerRadius::same(11), fade(theme.accent, 0.15));
    } else if t > 0.01 {
        p.rect_filled(pill, egui::CornerRadius::same(11), fade(theme.accent, 0.12 * t));
    }
    let stroke_c = if selected {
        theme.accent
    } else {
        mix(fade(egui::Color32::WHITE, 0.30), theme.accent, t)
    };
    p.rect_stroke(
        pill,
        egui::CornerRadius::same(11),
        egui::Stroke::new(1.1, fade(stroke_c, alpha)),
        egui::StrokeKind::Inside,
    );
    if enabled {
        resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    resp
}

/// pill 内居中文本。
fn draw_pill_text_c(p: &egui::Painter, pill: egui::Rect, text: &str, selected: bool, enabled: bool, theme: OscTheme) {
    let alpha = if enabled { 1.0 } else { 0.35 };
    let tc = if selected {
        theme.text
    } else {
        fade(theme.text_dim, alpha)
    };
    let gal = p.layout_no_wrap(text.to_string(), osc_font(12.0), tc);
    p.galley(
        egui::pos2(
            pill.center().x - gal.size().x / 2.0,
            pill.center().y - gal.size().y / 2.0,
        ),
        gal,
        tc,
    );
}

/// 弹窗字体选择行：左"字体"标签 + 默认 / 应用自带（固定窄 pill）+
/// 自定义字体（弹性宽 pill，显示已选字体名）。交互：无自定义路径时点击
/// "自定义字体"= 打开文件选择；有路径未选中时点击 = 切换到该字体；
/// 已选中时 pill 尾部叠修改按钮（自绘简笔铅笔，hover 圆底）另选文件。
/// 选中态 accent 描边 + 高亮底（载入胶囊同工艺）；禁用整行 35% 淡出不响应。
fn draw_font_choice_row(
    ui: &mut egui::Ui,
    id: &str,
    current: &dyplayer_store::DanmakuFontChoice,
    custom_path: Option<&Path>,
    enabled: bool,
    theme: OscTheme,
) -> Option<FontRowAction> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
    let p = ui.painter();
    let top = rect.center().y - 11.0;
    let pill_h = 22.0;
    let gap = 6.0;
    let mut action = None;

    // 左标签"字体"（与滑条行 label 同工艺）。
    let lc = if enabled {
        theme.text_dim
    } else {
        fade(theme.text_dim, 0.35)
    };
    p.text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        "字体",
        osc_font(12.5),
        lc,
    );
    let pills_left = rect.left() + 34.0;

    // 默认（固定 46px）。
    let default_pill =
        egui::Rect::from_min_size(egui::pos2(pills_left, top), egui::vec2(46.0, pill_h));
    let selected_default = matches!(current, dyplayer_store::DanmakuFontChoice::Default);
    let resp = draw_pill_bg(
        ui,
        p,
        default_pill,
        egui::Id::unique(id).with(0),
        selected_default,
        enabled,
        theme,
    );
    draw_pill_text_c(p, default_pill, "默认", selected_default, enabled, theme);
    if enabled && resp.clicked() {
        action = Some(FontRowAction::SelectDefault);
    }

    // 应用自带（固定 70px）。
    let app_pill = egui::Rect::from_min_size(
        egui::pos2(default_pill.right() + gap, top),
        egui::vec2(70.0, pill_h),
    );
    let selected_app = matches!(current, dyplayer_store::DanmakuFontChoice::App);
    let resp = draw_pill_bg(
        ui,
        p,
        app_pill,
        egui::Id::unique(id).with(1),
        selected_app,
        enabled,
        theme,
    );
    draw_pill_text_c(p, app_pill, "应用自带", selected_app, enabled, theme);
    if enabled && resp.clicked() {
        action = Some(FontRowAction::SelectApp);
    }

    // 自定义字体（弹性宽：吃掉剩余空间，为字体名留显示宽度）。
    let custom_pill = egui::Rect::from_min_max(
        egui::pos2(app_pill.right() + gap, top),
        egui::pos2(rect.right(), top + pill_h),
    );
    let selected_custom = matches!(current, dyplayer_store::DanmakuFontChoice::Custom(_));
    // 选中态尾部叠修改按钮（18×18），文本左对齐避让；否则居中。
    let edit_btn = if selected_custom {
        Some(egui::Rect::from_min_size(
            egui::pos2(custom_pill.right() - 21.0, top + 2.0),
            egui::vec2(18.0, 18.0),
        ))
    } else {
        None
    };
    let resp = draw_pill_bg(
        ui,
        p,
        custom_pill,
        egui::Id::unique(id).with(2),
        selected_custom,
        enabled,
        theme,
    );
    let alpha = if enabled { 1.0 } else { 0.35 };
    let tc = if selected_custom {
        theme.text
    } else {
        fade(theme.text_dim, alpha)
    };
    match custom_path {
        Some(path) => {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let max_w = custom_pill.width() - 14.0 - if edit_btn.is_some() { 21.0 } else { 0.0 };
            let label = truncate_chars(&stem, ((max_w / 12.0).floor() as usize).max(2));
            let gal = p.layout_no_wrap(label, osc_font(12.0), tc);
            let x = if edit_btn.is_some() {
                custom_pill.left() + 8.0
            } else {
                custom_pill.center().x - gal.size().x / 2.0
            };
            p.galley(
                egui::pos2(x, custom_pill.center().y - gal.size().y / 2.0),
                gal,
                tc,
            );
        }
        None => {
            draw_pill_text_c(p, custom_pill, "自定义字体", selected_custom, enabled, theme);
        }
    }
    if enabled && resp.clicked() {
        action = match (custom_path.is_some(), selected_custom) {
            (false, _) => Some(FontRowAction::PickCustomFile),
            (true, false) => Some(FontRowAction::SelectCustom),
            (true, true) => None, // 已选中：点击本体无操作（另选走修改按钮）
        };
    }

    // 修改按钮（仅自定义选中时）：自绘简笔铅笔 + hover 圆底。
    if let Some(btn) = edit_btn {
        let bresp = ui.interact(
            btn,
            egui::Id::unique(id).with("edit"),
            egui::Sense::click(),
        );
        let t = ui.ctx().animate_value_with_time(
            egui::Id::unique(id).with("edit-h"),
            (enabled && bresp.hovered()) as u8 as f32,
            0.08,
        );
        if t > 0.01 {
            p.circle_filled(btn.center(), 9.0, fade(egui::Color32::WHITE, 0.10 * t));
        }
        let c = mix(theme.text_dim, theme.text, t);
        let k = 3.2;
        // 斜杆（笔身）。
        p.line_segment(
            [
                egui::pos2(btn.center().x - k, btn.center().y + k),
                egui::pos2(btn.center().x + k - 1.4, btn.center().y - k + 1.4),
            ],
            egui::Stroke::new(1.3, c),
        );
        // 短横（笔头）。
        p.line_segment(
            [
                egui::pos2(btn.center().x - k - 1.0, btn.center().y + k - 2.4),
                egui::pos2(btn.center().x - k + 2.0, btn.center().y + k + 0.6),
            ],
            egui::Stroke::new(1.3, c),
        );
        if enabled && bresp.clicked() {
            action = Some(FontRowAction::PickCustomFile);
        }
        if enabled {
            bresp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    }

    action
}

/// 弹窗滑条行：label + 数值左侧的还原按钮 + 自绘轨道（OSC 迷你音量条
/// 工艺：命中区上下外扩，指针位置换算值；交互先行、绘制随后，本帧即显
/// 新值）。值偏离默认时还原按钮（↺）可点，恢复默认值。
/// 禁用时整行 35% 透明且不响应。返回值是否变化（调用方决定是否重 prepare）。
#[allow(clippy::too_many_arguments)]
fn draw_slider_row(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    min: f32,
    max: f32,
    default: f32,
    v: &mut f32,
    fmt: &dyn Fn(f32) -> String,
    enabled: bool,
    theme: OscTheme,
) -> bool {
    let w = ui.available_width();
    let (lrect, _) = ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
    let (trect, _) = ui.allocate_exact_size(egui::vec2(w, 22.0), egui::Sense::hover());
    let cy = trect.center().y;
    let x0 = trect.left();
    let x1 = trect.right();
    let hit = egui::Rect::from_min_max(egui::pos2(x0, cy - 9.0), egui::pos2(x1, cy + 9.0));
    let resp = ui.interact(hit, egui::Id::unique(id), egui::Sense::click_and_drag());
    let mut changed = false;
    if enabled
        && let Some(pt) = resp.interact_pointer_pos()
    {
        let nv = slider_hit_value(pt.x, x0, x1, min, max);
        if (nv - *v).abs() > 1e-6 {
            *v = nv;
            changed = true;
        }
    }
    let p = ui.painter();
    let lc = if enabled {
        theme.text_dim
    } else {
        fade(theme.text_dim, 0.35)
    };
    p.text(
        lrect.left_center(),
        egui::Align2::LEFT_CENTER,
        label,
        osc_font(12.5),
        lc,
    );
    let val_c = if enabled {
        theme.text
    } else {
        fade(theme.text, 0.35)
    };
    let gal = p.layout_no_wrap(fmt(*v), osc_font(12.5), val_c);

    // 还原按钮（数值左侧 18×18）：值即默认或行禁用时淡显不可点。
    let off_default = (*v - default).abs() > 1e-6;
    let reset_active = enabled && off_default;
    let reset_rect = egui::Rect::from_min_size(
        egui::pos2(
            lrect.right() - gal.size().x - 6.0 - 18.0,
            lrect.center().y - 9.0,
        ),
        egui::vec2(18.0, 18.0),
    );
    let rresp = ui.interact(
        reset_rect,
        egui::Id::unique(id).with("reset"),
        egui::Sense::click(),
    );
    let rt = ui.ctx().animate_value_with_time(
        egui::Id::unique(id).with("reset-h"),
        (reset_active && rresp.hovered()) as u8 as f32,
        0.08,
    );
    if rt > 0.01 {
        p.circle_filled(
            reset_rect.center(),
            9.0,
            fade(egui::Color32::WHITE, 0.10 * rt),
        );
    }
    let reset_c = if reset_active {
        mix(theme.text_dim, theme.text, rt.max(0.25))
    } else {
        fade(theme.text_dim, 0.35)
    };
    paint_reset_icon(p, reset_rect.center(), 5.0, reset_c);
    if reset_active {
        if rresp.clicked() {
            *v = default;
            changed = true;
        }
        rresp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("恢复默认");
    }

    p.galley(
        egui::pos2(
            lrect.right() - gal.size().x,
            lrect.center().y - gal.size().y / 2.0,
        ),
        gal,
        val_c,
    );
    p.rect_filled(
        egui::Rect::from_min_max(egui::pos2(x0, cy - 1.5), egui::pos2(x1, cy + 1.5)),
        egui::CornerRadius::same(2),
        fade(egui::Color32::WHITE, if enabled { 0.16 } else { 0.07 }),
    );
    let frac = if max > min {
        ((*v - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if frac > 0.001 {
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, cy - 1.5),
                egui::pos2(x0 + (x1 - x0) * frac, cy + 1.5),
            ),
            egui::CornerRadius::same(2),
            if enabled {
                theme.accent
            } else {
                fade(theme.accent, 0.35)
            },
        );
    }
    let ht = ui.ctx().animate_value_with_time(
        egui::Id::unique(id).with("h"),
        (enabled && (resp.hovered() || resp.is_pointer_button_down_on())) as u8 as f32,
        0.08,
    );
    let r = 5.0 + 1.2 * ht;
    let hx = x0 + (x1 - x0) * frac;
    p.circle_filled(
        egui::pos2(hx, cy),
        r,
        fade(egui::Color32::WHITE, if enabled { 1.0 } else { 0.35 }),
    );
    p.circle_stroke(
        egui::pos2(hx, cy),
        r + 1.5,
        egui::Stroke::new(
            1.0,
            if enabled {
                theme.accent
            } else {
                fade(theme.accent, 0.35)
            },
        ),
    );
    ui.add_space(5.0);
    changed
}

/// 还原按钮的 ↺ 图标：3/4 圆弧 + 终点箭头两翼（纯几何自绘，无字形依赖，
/// 避免 ↺ 在 HarmonyOS/图标字体中缺字出现豆腐块）。
fn paint_reset_icon(p: &egui::Painter, c: egui::Pos2, r: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.4, color);
    let a0 = 270.0_f32.to_radians();
    let span = 270.0_f32.to_radians();
    let n = 8usize;
    let pt = |i: usize| {
        let a = a0 + span * (i as f32 / n as f32);
        egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
    };
    for i in 0..n {
        p.line_segment([pt(i), pt(i + 1)], stroke);
    }
    // 箭头两翼：从弧终点沿切线方向（角度递增 = (−sinθ, cosθ)）反向张开。
    let ta = a0 + span;
    let dir = egui::vec2(-ta.sin(), ta.cos());
    let tip = pt(n);
    for rot in [150.0_f32.to_radians(), -150.0_f32.to_radians()] {
        let (s, co) = rot.sin_cos();
        let w = egui::vec2(dir.x * co - dir.y * s, dir.x * s + dir.y * co);
        p.line_segment(
            [tip, egui::pos2(tip.x + w.x * 3.2, tip.y + w.y * 3.2)],
            stroke,
        );
    }
}

/// 延迟数值显示：0 → "0:00"；其余 ±m:ss（滑条区间 ±600s）。
fn fmt_delay(v: f32) -> String {
    let s = v.abs().round() as i64;
    if s == 0 {
        "0:00".into()
    } else if v < 0.0 {
        format!("-{}:{:02}", s / 60, s % 60)
    } else {
        format!("+{}:{:02}", s / 60, s % 60)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_path_replaces_extension() {
        assert_eq!(
            xml_path_for_video(Path::new(r"D:\video\ep01.mp4")),
            PathBuf::from(r"D:\video\ep01.xml")
        );
        assert_eq!(
            xml_path_for_video(Path::new("movie.mkv")),
            PathBuf::from("movie.xml")
        );
    }

    #[test]
    fn settings_card_rect_anchors_above_button() {
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let size = egui::vec2(300.0, 365.0);
        let r = settings_card_rect(screen, Some(egui::pos2(400.0, 500.0)), size);
        assert!((r.center().x - 400.0).abs() < 1e-6);
        assert!((r.bottom() - 500.0).abs() < 1e-6);
    }

    #[test]
    fn settings_card_rect_clamps_to_screen() {
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let size = egui::vec2(300.0, 365.0);
        let r = settings_card_rect(screen, Some(egui::pos2(40.0, 500.0)), size);
        assert!((r.left() - 8.0).abs() < 1e-6);
        let r = settings_card_rect(screen, Some(egui::pos2(790.0, 500.0)), size);
        assert!((r.right() - 792.0).abs() < 1e-6);
        let r = settings_card_rect(screen, Some(egui::pos2(400.0, 50.0)), size);
        assert!((r.top() - 8.0).abs() < 1e-6);
    }

    #[test]
    fn settings_card_rect_falls_back_to_bottom_right() {
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let size = egui::vec2(300.0, 365.0);
        let r = settings_card_rect(screen, None, size);
        assert!((r.right() - (screen.right() - 24.0)).abs() < 1e-6);
        assert!((r.bottom() - (screen.bottom() - 24.0)).abs() < 1e-6);
    }

    #[test]
    fn slider_hit_value_maps_and_clamps() {
        assert!((slider_hit_value(50.0, 0.0, 100.0, 0.0, 1.0) - 0.5).abs() < 1e-6);
        assert!(slider_hit_value(-10.0, 0.0, 100.0, 0.0, 1.0).abs() < 1e-6);
        assert!((slider_hit_value(120.0, 0.0, 100.0, 0.0, 1.0) - 1.0).abs() < 1e-6);
        assert!((slider_hit_value(50.0, 0.0, 100.0, 2.0, 4.0) - 3.0).abs() < 1e-6);
        assert!((slider_hit_value(50.0, 100.0, 100.0, 2.0, 4.0) - 2.0).abs() < 1e-6);
        assert!((slider_hit_value(50.0, 0.0, 100.0, 4.0, 2.0) - 4.0).abs() < 1e-6);
    }


    #[test]
    fn missing_xml_means_no_danmaku_session() {
        let mut overlay = DanmakuOverlay::new();
        let missing = std::env::temp_dir().join("dyplayer-test-no-such-file.mp4");
        assert_eq!(overlay.on_file_loaded(&missing, None), None);
        assert!(overlay.session_is_none_for_test());
    }

    #[test]
    fn valid_xml_loads_session_when_font_available() {
        let xml = std::env::temp_dir().join("dyplayer-test-danmaku.mp4.xml");
        std::fs::write(&xml, r#"<d p="0.0,1,25,16777215,0,0,0,0,0">hello</d>"#).unwrap();
        let video = xml.with_extension("mp4");
        let mut overlay = DanmakuOverlay::new();
        let used = overlay.on_file_loaded(&video, None);
        assert_eq!(used.as_deref(), Some(xml.as_path()));
        // R12-B1：on_file_loaded 异步化，等后台线程完成再检查 session。
        overlay.flush_pending_xml_load_for_test();
        // 可用性判断与 ensure_session 同口径（默认选择的候选链）。
        if DanmakuFont::from_paths(&font_candidates(
            &dyplayer_store::DanmakuFontChoice::Default,
        ))
        .is_some()
        {
            assert!(!overlay.session_is_none_for_test());
            assert_eq!(overlay.item_count(), 1);
        } else {
            assert!(overlay.session_is_none_for_test());
        }
        let _ = std::fs::remove_file(&xml);
    }

    #[test]
    fn preferred_binding_wins_and_falls_back_to_sibling() {
        // 手动绑定文件存在：优先于同名 xml
        let dir = std::env::temp_dir();
        let video = dir.join("dyplayer-test-pref.mp4");
        let sibling = dir.join("dyplayer-test-pref.xml");
        let bound = dir.join("dyplayer-test-pref-manual.xml");
        std::fs::write(&sibling, r#"<d p="0.0,1,25,16777215,0,0,0,0,0">sib</d>"#).unwrap();
        std::fs::write(&bound, r#"<d p="0.0,1,25,16777215,0,0,0,0,0">bound</d>"#).unwrap();
        let mut overlay = DanmakuOverlay::new();
        let used = overlay.on_file_loaded(&video, Some(&bound));
        assert_eq!(used.as_deref(), Some(bound.as_path()));
        // 绑定文件被删：回退同名 xml
        let _ = std::fs::remove_file(&bound);
        let used2 = overlay.on_file_loaded(&video, Some(&bound));
        assert_eq!(used2.as_deref(), Some(sibling.as_path()));
        let _ = std::fs::remove_file(&sibling);
    }

    #[test]
    fn danmaku_config_defaults_match_overlay_defaults() {
        // config.toml [danmaku] 默认值须与播放页 DEFAULT_* 一致（防漂移），
        // 否则首次启动 apply_config 会偏离用户预期的新装默认。
        let cfg = dyplayer_store::DanmakuConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.opacity, DEFAULT_OPACITY);
        assert_eq!(cfg.font_size, DEFAULT_FONT_SIZE);
        assert_eq!(cfg.display_area as f64, DEFAULT_AREA);
        assert_eq!(cfg.speed as f64, DEFAULT_SPEED);
        assert_eq!(cfg.font, dyplayer_store::DanmakuFontChoice::Default);
        assert_eq!(cfg.gradient, DEFAULT_GRADIENT);
        assert_eq!(cfg.gradient_inset as f64, DEFAULT_GRADIENT_INSET);
        assert_eq!(cfg.outline_width as f64, DEFAULT_OUTLINE);
        assert_eq!(cfg.show_top, DEFAULT_SHOW_TOP);
        assert_eq!(cfg.show_bottom, DEFAULT_SHOW_BOTTOM);
    }

    #[test]
    fn apply_config_then_export_roundtrip() {
        let mut overlay = DanmakuOverlay::new();
        let cfg = dyplayer_store::DanmakuConfig {
            enabled: false,
            opacity: 0.4,
            font_size: 48.0,
            display_area: 0.25,
            speed: 2.0,
            font: dyplayer_store::DanmakuFontChoice::Custom("C:\\Fonts\\my.ttf".into()),
            gradient: true,
            gradient_inset: 0.25,
            outline_width: 3.5,
            show_top: false,
            show_bottom: true,
        };
        overlay.apply_config(&cfg);
        assert!(!overlay.enabled);
        assert_eq!(overlay.opacity, 0.4);
        assert_eq!(overlay.font_size_cfg, 48.0);
        assert_eq!(overlay.display_area_cfg, 0.25);
        assert_eq!(overlay.speed_cfg, 2.0);
        assert!(overlay.gradient_cfg);
        assert_eq!(overlay.gradient_inset_cfg, 0.25);
        assert_eq!(overlay.outline_cfg, 3.5);
        assert!(!overlay.show_top_cfg);
        assert!(overlay.show_bottom_cfg);
        assert_eq!(
            overlay.font_choice_cfg,
            dyplayer_store::DanmakuFontChoice::Custom("C:\\Fonts\\my.ttf".into())
        );
        // Custom 选择恢复时同步 custom_font_path（供"自定义"pill 切回）。
        assert_eq!(
            overlay.custom_font_path.as_deref(),
            Some(Path::new(r"C:\Fonts\my.ttf"))
        );
        let out = overlay.export_config();
        assert!(!out.enabled);
        assert_eq!(out.opacity, 0.4);
        assert_eq!(out.font_size, 48.0);
        assert_eq!(out.display_area, 0.25);
        assert_eq!(out.speed, 2.0);
        assert!(out.gradient);
        assert_eq!(out.gradient_inset, 0.25);
        assert_eq!(out.outline_width, 3.5);
        assert!(!out.show_top);
        assert!(out.show_bottom);
        assert_eq!(
            out.font,
            dyplayer_store::DanmakuFontChoice::Custom("C:\\Fonts\\my.ttf".into())
        );
    }

    #[test]
    fn fixed_type_visible_filters_top_bottom_only() {
        use dyplayer_danmaku::dfm_core::model::DanmakuType as T;
        let top = T::FixTop as i32;
        let bottom = T::FixBottom as i32;
        let scroll = T::ScrollRL as i32;
        // 开关各自只影响对应类型。
        assert!(!fixed_type_visible(top, false, true));
        assert!(fixed_type_visible(top, true, false));
        assert!(!fixed_type_visible(bottom, true, false));
        assert!(fixed_type_visible(bottom, false, true));
        // 滚动弹幕不受顶/底开关影响（由"显示弹幕"主开关整体控制）。
        assert!(fixed_type_visible(scroll, false, false));
    }

    #[test]
    fn load_xml_file_reports_missing_file() {
        let mut overlay = DanmakuOverlay::new();
        let missing = std::env::temp_dir().join("dyplayer-test-no-such-danmaku.xml");
        let err = overlay.load_xml_file(&missing).unwrap_err();
        assert!(err.contains("读取弹幕文件失败"), "got: {err}");
        assert!(overlay.session_is_none_for_test());
    }

    #[test]
    fn load_xml_file_rejects_empty_file() {
        let xml = std::env::temp_dir().join("dyplayer-test-danmaku-empty.xml");
        std::fs::write(&xml, "   \n").unwrap();
        let mut overlay = DanmakuOverlay::new();
        let err = overlay.load_xml_file(&xml).unwrap_err();
        assert!(err.contains("弹幕文件为空"), "got: {err}");
        assert!(overlay.session_is_none_for_test());
        let _ = std::fs::remove_file(&xml);
    }

    #[test]
    fn load_xml_file_rejects_xml_without_items() {
        let xml = std::env::temp_dir().join("dyplayer-test-danmaku-noitems.xml");
        std::fs::write(&xml, "<i></i>").unwrap();
        let mut overlay = DanmakuOverlay::new();
        match overlay.load_xml_file(&xml) {
            Err(e) => {
                assert!(
                    e.contains("没有可用弹幕") || e.contains("系统字体"),
                    "got: {e}"
                );
            }
            Ok(_) => panic!("item-less xml must not load"),
        }
        let _ = std::fs::remove_file(&xml);
    }

    #[test]
    fn load_xml_file_loads_items_when_font_available() {
        let xml = std::env::temp_dir().join("dyplayer-test-danmaku-manual.xml");
        std::fs::write(&xml, r#"<d p="0.0,1,25,16777215,0,0,0,0,0">hello</d>"#).unwrap();
        let mut overlay = DanmakuOverlay::new();
        if DanmakuFont::from_paths(&font_candidates(
            &dyplayer_store::DanmakuFontChoice::Default,
        ))
        .is_some()
        {
            assert_eq!(overlay.load_xml_file(&xml).unwrap(), 1);
            assert_eq!(overlay.item_count(), 1);
            assert!(!overlay.session_is_none_for_test());
        } else {
            assert!(overlay.load_xml_file(&xml).is_err());
        }
        let _ = std::fs::remove_file(&xml);
    }

    #[test]
    fn load_xml_file_replaces_previous_track() {
        let xml = std::env::temp_dir().join("dyplayer-test-danmaku-replace.xml");
        let mut overlay = DanmakuOverlay::new();
        if DanmakuFont::from_paths(&font_candidates(
            &dyplayer_store::DanmakuFontChoice::Default,
        ))
        .is_none()
        {
            return;
        }
        std::fs::write(&xml, r#"<d p="0.0,1,25,16777215,0,0,0,0,0">one</d>"#).unwrap();
        assert_eq!(overlay.load_xml_file(&xml).unwrap(), 1);
        std::fs::write(
            &xml,
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0">a</d><d p="1.0,1,25,16777215,0,0,0,0,0">b</d>"#,
        )
        .unwrap();
        assert_eq!(overlay.load_xml_file(&xml).unwrap(), 2);
        assert_eq!(overlay.item_count(), 2);
        let _ = std::fs::remove_file(&xml);
    }

    #[test]
    fn on_media_change_clears_manual_load_state() {
        let mut overlay = DanmakuOverlay::new();
        overlay.pending_pick = true;
        overlay.last_load_result = Some(Ok(3));
        overlay.pending_font_pick = true;
        overlay.last_font_result = Some(Ok("MyFont".into()));
        overlay.on_media_change();
        assert!(!overlay.pending_pick);
        assert!(overlay.last_load_result.is_none());
        assert!(!overlay.pending_font_pick);
        assert!(overlay.last_font_result.is_none());
        // 字体路径是会话级记忆，换集不清。
        overlay.custom_font_path = Some(PathBuf::from("C:/f.ttf"));
        overlay.on_media_change();
        assert!(overlay.custom_font_path.is_some());
    }

    #[test]
    fn fmt_delay_formats_minutes_seconds() {
        assert_eq!(fmt_delay(0.0), "0:00");
        assert_eq!(fmt_delay(0.4), "0:00");
        assert_eq!(fmt_delay(30.0), "+0:30");
        assert_eq!(fmt_delay(90.0), "+1:30");
        assert_eq!(fmt_delay(600.0), "+10:00");
        assert_eq!(fmt_delay(-125.0), "-2:05");
        assert_eq!(fmt_delay(-600.0), "-10:00");
    }

    #[test]
    fn new_matches_default_constants() {
        let o = DanmakuOverlay::new();
        assert_eq!(o.opacity, DEFAULT_OPACITY);
        assert_eq!(o.delay_seconds, DEFAULT_DELAY);
        assert_eq!(o.font_size_cfg, DEFAULT_FONT_SIZE);
        assert_eq!(o.display_area_cfg, DEFAULT_AREA);
        assert_eq!(o.speed_cfg, DEFAULT_SPEED);
        assert_eq!(o.gradient_cfg, DEFAULT_GRADIENT);
        assert_eq!(o.gradient_inset_cfg, DEFAULT_GRADIENT_INSET);
        assert_eq!(o.outline_cfg, DEFAULT_OUTLINE);
        assert_eq!(o.font_choice_cfg, dyplayer_store::DanmakuFontChoice::Default);
        assert!(o.custom_font_path.is_none());
    }

    #[test]
    fn font_candidates_start_with_choice_and_fall_back() {
        // Default：不指定字体，链 = 系统候选链（与旧版 from_system 一致）。
        let default = font_candidates(&dyplayer_store::DanmakuFontChoice::Default);
        assert!(default.first().is_some_and(|p| p.ends_with("msyh.ttc")));
        assert!(default.iter().any(|p| p.ends_with("simhei.ttf")));
        assert!(default.iter().any(|p| p.ends_with("simsun.ttc")));
        // App：链首 = HarmonyOS 候选（与 UI 主字体同源），后接系统回退。
        let app = font_candidates(&dyplayer_store::DanmakuFontChoice::App);
        assert!(
            app.first().is_some_and(|p| p.to_string_lossy().contains("HarmonyOS")),
            "App chain must lead with HarmonyOS candidates, got {app:?}"
        );
        assert!(app.iter().any(|p| p.ends_with("msyh.ttc")));
        // Custom：链首 = 用户文件，交叉回退兜底（文件被删不判死）。
        let custom = font_candidates(&dyplayer_store::DanmakuFontChoice::Custom(
            "C:\\My\\font.ttf".into(),
        ));
        assert!(custom.first().is_some_and(|p| p == &PathBuf::from("C:\\My\\font.ttf")));
        assert!(custom.iter().any(|p| p.ends_with("msyh.ttc")));
        // 无重复路径。
        for chain in [default, app, custom] {
            let mut seen = std::collections::HashSet::new();
            assert!(chain.iter().all(|p| seen.insert(p.clone())));
        }
    }

    #[test]
    fn truncate_chars_appends_ellipsis_when_long() {
        assert_eq!(truncate_chars("abcdef", 10), "abcdef");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("中文字体名", 3), "中文字…");
        assert_eq!(truncate_chars("", 3), "");
        // 按字符计非字节：CJK 3 字符截断不劈开多字节序列。
        assert_eq!(truncate_chars("中文字体名", 4).chars().count(), 5);
    }

    #[test]
    fn set_font_choice_updates_cfg_without_session() {
        // session 未建立：仅记录选择，重置不可用标记供下次 ensure_session 重试。
        let mut overlay = DanmakuOverlay::new();
        overlay.font_unavailable = true;
        overlay.set_font_choice(dyplayer_store::DanmakuFontChoice::Custom(
            "C:\\My\\font.ttf".into(),
        ));
        assert_eq!(
            overlay.font_choice_cfg,
            dyplayer_store::DanmakuFontChoice::Custom("C:\\My\\font.ttf".into())
        );
        assert!(!overlay.font_unavailable);
        assert!(overlay.session_is_none_for_test());
        let out = overlay.export_config();
        assert_eq!(
            out.font,
            dyplayer_store::DanmakuFontChoice::Custom("C:\\My\\font.ttf".into())
        );
    }

    #[test]
    fn set_font_choice_hot_swaps_active_session() {
        let font_ok = DanmakuFont::from_paths(&font_candidates(
            &dyplayer_store::DanmakuFontChoice::Default,
        ))
        .is_some();
        if !font_ok {
            eprintln!("skip: no danmaku font available");
            return;
        }
        let xml = std::env::temp_dir().join("dyplayer-test-danmaku-font.xml");
        std::fs::write(&xml, r#"<d p="0.0,1,25,16777215,0,0,0,0,0">hello</d>"#).unwrap();
        let mut overlay = DanmakuOverlay::new();
        assert_eq!(overlay.load_xml_file(&xml).unwrap(), 1);
        assert!(overlay.session.is_some());

        // 切到 App 再切回默认：候选链交叉回退保证加载成功。
        overlay.set_font_choice(dyplayer_store::DanmakuFontChoice::App);
        assert_eq!(overlay.font_choice_cfg, dyplayer_store::DanmakuFontChoice::App);
        assert!(overlay.session.is_some());
        assert_eq!(overlay.item_count(), 1);

        overlay.set_font_choice(dyplayer_store::DanmakuFontChoice::Default);
        assert_eq!(overlay.font_choice_cfg, dyplayer_store::DanmakuFontChoice::Default);
        assert!(overlay.session.is_some());
        assert_eq!(overlay.item_count(), 1);
        let _ = std::fs::remove_file(&xml);
    }
}
