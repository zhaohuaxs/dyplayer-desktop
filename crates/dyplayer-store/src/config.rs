use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub ui: UiConfig,
    pub player: PlayerConfig,
    pub danmaku: DanmakuConfig,
    pub engine: EngineConfig,
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: ThemeMode,
    pub side_panel_width: f32,
    /// 启动时进入的默认视图（媒体库 / 播放页）。
    pub startup_view: StartupView,
    /// 主题色板索引（0-3：橙/蓝/绿/紫；越界解析按 0 处理）。
    pub accent: u8,
    /// UI 主字体（应用自带 / 系统字体显示名，如 "微软雅黑"）。
    pub font: FontChoice,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            side_panel_width: 240.0,
            startup_view: StartupView::default(),
            accent: 3,
            font: FontChoice::default(),
        }
    }
}

/// 全局明暗主题模式：跟随系统 / 浅色 / 深色。
/// 旧配置中的 "dark"/"light" 字符串可正常解析；未知值回退跟随系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    #[serde(other)]
    System,
}

/// 应用启动时进入的默认视图。
/// alias 容旧版写入的大写 "Library"/"Player"（历史上曾以 variant 名原样
/// 落盘；无 alias 时 serde 解析失败会触发整份 config 回退默认，殃及
/// engine.render_path 等全部字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StartupView {
    #[default]
    #[serde(alias = "Library")]
    Library,
    #[serde(alias = "Player")]
    Player,
}

/// UI 主字体选择：应用自带（HarmonyOS Sans SC）或系统字体（显示名）。
/// 系统候选由 app 层按常见中文字体 + 文件存在过滤（检测字体 CJK 支持
/// 需解析 cmap，成本高不做，直接内置中文字体候选表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FontChoice {
    #[default]
    App,
    System(String),
}

/// 弹幕字体选择：默认（不指定字体，沿系统候选链回退，与旧版行为一致）/
/// 包内 HarmonyOS Sans SC Medium / 自定义字体文件（绝对路径）。
/// 加载失败时 app 层沿候选链交叉回退（保证弹幕可用）；未知配置值（如已
/// 移除的 yahei/simhei）经 `#[serde(other)]` 回退默认，不炸整份配置加载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DanmakuFontChoice {
    App,
    /// 用户自选字体文件的绝对路径（设置弹窗 rfd 选择）。
    Custom(String),
    #[default]
    #[serde(other)]
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub auto_next: bool,
    pub seek_step_fwd: i64,
    pub seek_step_back: i64,
    pub pref_audio_lang: String,
    pub pref_sub_lang: String,
    pub sub_disabled_by_default: bool,
    /// 播放器底部统计信息条（时间/帧率等 HUD），默认不显示。
    pub show_stats_hud: bool,
    /// 上次退出时的音量（0-100，会话启动注入 mpv）。
    pub volume: i64,
    /// 视频配速：true = 按屏幕刷新率平滑配速（mpv `video-sync=display-resample`，
    /// 弹幕满帧率）；false = 音频时钟配速（`video-sync=audio`，省 CPU，作高码率
    /// 卡顿对照/低配机选项）。运行时切换即时生效并记忆。
    pub display_pacing: bool,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            auto_next: true,
            seek_step_fwd: 85,
            seek_step_back: 10,
            pref_audio_lang: String::new(),
            pref_sub_lang: String::new(),
            sub_disabled_by_default: false,
            show_stats_hud: false,
            volume: 100,
            display_pacing: true,
        }
    }
}

/// 弹幕全局设置（播放页弹幕设置弹窗持久化项）。
/// 默认值与 app 层 DanmakuOverlay 的 DEFAULT_* 常量一致（有跨 crate
/// 一致性测试防漂移）。弹幕延迟不在此：它属于 (视频, 弹幕文件) 对，
/// 按条目存 SQLite danmaku_bindings 表。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DanmakuConfig {
    pub enabled: bool,
    pub opacity: f32,
    pub font_size: f32,
    pub display_area: f32,
    pub speed: f32,
    pub font: DanmakuFontChoice,
    /// dy 扩展渐变开关（弹幕 s 属性渐变；默认关）。
    pub gradient: bool,
    /// 渐变端点对称收缩量（0-0.25）：start = inset、end = 1 - inset，
    /// 0 = 全区间（现状）；渲染参数即时生效。
    pub gradient_inset: f32,
    /// 描边宽度 profile（0-4；0 = 无描边）。
    pub outline_width: f32,
    /// 顶部弹幕（FixTop）显示开关（查询侧过滤即时生效）。
    pub show_top: bool,
    /// 底部弹幕（FixBottom）显示开关（查询侧过滤即时生效）。
    pub show_bottom: bool,
}

impl Default for DanmakuConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            opacity: 1.0,
            font_size: 30.0,
            display_area: 1.0,
            speed: 1.0,
            font: DanmakuFontChoice::Default,
            gradient: false,
            gradient_inset: 0.0,
            outline_width: 2.0,
            show_top: true,
            show_bottom: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub mode: String,
    pub mpv_path: String,
    pub libmpv_path: String,
    pub ipc_name: String,
    pub hwdec: String,
    /// 渲染路径：`sw`（默认，mpv 软渲染 + wgpu 纹理上传）或 `gpu`
    /// （mpv `wid` 自管 D3D11 渲染，宿主只出窗口、呈现由 mpv 承担；M1 实验路径，
    /// 见 docs/poc-wid-gpu-render.md）。切换属会话重建型。
    pub render_path: String,
    /// 画质档位：`high`（默认，mpv gpu-hq profile 零回归）/ `balanced`
    /// （spline36 采样器）/ `low-power`（bilinear + 关 sigmoid/interpolation，
    /// 弱 iGPU 如 N2940 Gen7 避免 GPU 反压解码）。切换属会话重建型
    /// （初始化期 mpv 选项，运行时改需重建会话）。未知值归入 high。
    pub quality_preset: String,
    pub extra_args: Vec<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            mode: "embedded".into(),
            mpv_path: String::new(),
            libmpv_path: String::new(),
            ipc_name: String::new(),
            hwdec: "auto-safe".into(),
            render_path: "sw".into(),
            quality_preset: "high".into(),
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub dir: Option<PathBuf>,
    pub max_mb: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_mb: 512,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("TOML serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl AppConfig {
    pub fn config_dir() -> anyhow::Result<PathBuf> {
        let dir = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        Ok(dir.join("dyplayer"))
    }

    pub fn data_dir() -> anyhow::Result<PathBuf> {
        let dir = dirs::data_local_dir().ok_or_else(|| anyhow::anyhow!("no local data dir"))?;
        Ok(dir.join("dyplayer"))
    }

    pub fn load_or_default() -> anyhow::Result<Self> {
        let path = Self::config_dir()?.join("config.toml");
        if !path.exists() {
            let cfg = Self::default();
            cfg.save(&path)?;
            return Ok(cfg);
        }
        Ok(Self::load_from(&path))
    }

    /// 从指定路径加载；文件损坏/解析失败时备份原文件并回退默认，
    /// 保证应用始终可启动（可用性优先，坏配置留档可查）。
    fn load_from(path: &std::path::Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(
                    "read config {} failed ({err}); using defaults",
                    path.display()
                );
                return Self::default();
            }
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                let backup = path.with_extension("toml.bak");
                if let Err(rename_err) = std::fs::rename(path, &backup) {
                    // rename 失败（目标已存在/权限不足）时改用 copy + 时间戳后缀，
                    // 保留损坏配置供事后分析；dyplayer-store 不依赖 chrono，用 std 时间。
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let fname = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("config");
                    let backup2 = path.with_file_name(format!("{fname}.{ts}.bak"));
                    if let Err(copy_err) = std::fs::copy(path, &backup2) {
                        tracing::error!(
                            "config {} is corrupt ({err}); \
                             backup rename failed ({rename_err}) and copy to {} failed ({copy_err}); \
                             original will be overwritten",
                            path.display(),
                            backup2.display(),
                        );
                    } else {
                        tracing::warn!(
                            "config {} is corrupt ({err}); \
                             rename failed ({rename_err}), copied to {}, reset to defaults",
                            path.display(),
                            backup2.display(),
                        );
                    }
                } else {
                    tracing::warn!(
                        "config {} is corrupt ({err}); backed up to {}, reset to defaults",
                        path.display(),
                        backup.display()
                    );
                }
                let cfg = Self::default();
                if let Err(save_err) = cfg.save(path) {
                    tracing::warn!("write default config failed: {save_err}");
                }
                cfg
            }
        }
    }

    /// 原子写：write-to-temp + 同目录 rename，避免写入中途崩溃产生半截
    /// TOML 导致下次启动解析失败丢全部设置。同目录 rename 保证同卷原子。
    /// 若需覆盖掉电场景（不仅是进程崩溃），可在 write 后追加 `f.sync_all()`。
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => std::path::Path::new("."),
        };
        std::fs::create_dir_all(parent)?;

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("config");
        let tmp = parent.join(format!(".{name}.tmp"));

        let text = toml::to_string_pretty(self)?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_config_defaults_stats_hud_off() {
        assert!(!PlayerConfig::default().show_stats_hud);
    }

    #[test]
    fn player_config_toml_roundtrip_keeps_stats_hud() {
        let mut cfg = AppConfig::default();
        cfg.player.show_stats_hud = true;
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert!(parsed.player.show_stats_hud);
    }

    #[test]
    fn player_config_missing_field_falls_back_to_default() {
        // 旧版本配置文件没有 show_stats_hud 字段，serde(default) 应回退默认值 false
        let text = r#"
[player]
auto_next = false
seek_step_fwd = 30
seek_step_back = 5
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert!(!parsed.player.show_stats_hud);
        assert!(!parsed.player.auto_next);
        assert_eq!(parsed.player.seek_step_fwd, 30);
    }

    #[test]
    fn player_config_defaults_display_pacing_on() {
        assert!(PlayerConfig::default().display_pacing);
    }

    #[test]
    fn player_config_missing_display_pacing_falls_back_on() {
        // 旧版本配置文件没有 display_pacing 字段，serde(default) 应回退默认值 true
        // （保持现状：display-resample 平滑配速）。
        let text = r#"
[player]
auto_next = false
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert!(parsed.player.display_pacing);
    }

    #[test]
    fn player_config_roundtrip_keeps_display_pacing_off() {
        let mut cfg = AppConfig::default();
        cfg.player.display_pacing = false;
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert!(!parsed.player.display_pacing);
    }

    #[test]
    fn engine_config_defaults_render_path_sw() {
        assert_eq!(EngineConfig::default().render_path, "sw");
    }

    #[test]
    fn engine_config_missing_render_path_falls_back_to_sw() {
        // 旧版本配置文件没有 render_path 字段，serde(default) 应回退 "sw"
        // （默认 SW 渲染路径，零行为变化）。
        let text = r#"
[engine]
hwdec = "no"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.engine.render_path, "sw");
    }

    #[test]
    fn engine_config_roundtrip_keeps_render_path_gpu() {
        let mut cfg = AppConfig::default();
        cfg.engine.render_path = "gpu".into();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.engine.render_path, "gpu");
    }

    #[test]
    fn engine_config_defaults_quality_preset_high() {
        assert_eq!(EngineConfig::default().quality_preset, "high");
    }

    #[test]
    fn engine_config_missing_quality_preset_falls_back_to_high() {
        // 旧版本配置文件没有 quality_preset 字段，serde(default) 应回退 "high"
        //（零回归：高档 = mpv 默认 gpu-hq profile，无额外参数注入）。
        let text = r#"
[engine]
hwdec = "no"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.engine.quality_preset, "high");
    }

    #[test]
    fn engine_config_roundtrip_keeps_quality_preset_low_power() {
        let mut cfg = AppConfig::default();
        cfg.engine.quality_preset = "low-power".into();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.engine.quality_preset, "low-power");
    }

    #[test]
    fn ui_config_roundtrip_keeps_startup_view() {
        let mut cfg = AppConfig::default();
        cfg.ui.startup_view = StartupView::Player;
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.startup_view, StartupView::Player);
    }

    #[test]
    fn ui_config_legacy_startup_view_capitalized_parses() {
        // 旧版落盘的大写 variant 名：alias 容错解析，不得触发整份默认回退
        // （否则 engine.render_path 等字段一并被吞）。
        let text = r#"
[ui]
startup_view = "Library"

[engine]
render_path = "gpu"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.startup_view, StartupView::Library);
        assert_eq!(parsed.engine.render_path, "gpu");
    }

    #[test]
    fn ui_config_missing_startup_view_falls_back_to_library() {
        // 旧版本配置没有 [ui].startup_view 字段，serde(default) 应回退 Library
        let text = r#"
[ui]
theme = "light"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.startup_view, StartupView::Library);
        assert_eq!(parsed.ui.theme, ThemeMode::Light);
    }

    #[test]
    fn ui_config_legacy_theme_dark_parses() {
        // 旧版本 [ui].theme 是字符串 "dark"，枚举化后须继续解析为 Dark
        let text = r#"
[ui]
theme = "dark"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.theme, ThemeMode::Dark);
    }

    #[test]
    fn ui_config_unknown_theme_falls_back_to_system() {
        let text = r#"
[ui]
theme = "nonsense"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.theme, ThemeMode::System);
    }

    #[test]
    fn ui_config_missing_theme_defaults_to_system() {
        let text = r#"
[ui]
side_panel_width = 300.0
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.theme, ThemeMode::System);
    }

    #[test]
    fn ui_config_theme_roundtrip_serializes_lowercase() {
        let mut cfg = AppConfig::default();
        cfg.ui.theme = ThemeMode::Light;
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("theme = \"light\""));
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme, ThemeMode::Light);
    }

    fn temp_cfg_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dyplayer-cfg-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_from_valid_toml_returns_config() {
        let dir = temp_cfg_dir("valid");
        let path = dir.join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.ui.theme = ThemeMode::Dark;
        cfg.save(&path).unwrap();
        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.ui.theme, ThemeMode::Dark);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_corrupt_toml_falls_back_to_default_with_backup() {
        let dir = temp_cfg_dir("corrupt");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui\nbroken =").unwrap();
        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.ui.startup_view, StartupView::Library);
        assert!(dir.join("config.toml.bak").exists(), "backup file missing");
        assert!(path.exists(), "default config not rewritten");
        let text = std::fs::read_to_string(&path).unwrap();
        let reparsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(reparsed.ui.startup_view, StartupView::Library);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_invalid_enum_value_falls_back_to_default() {
        // StartupView 合法值是 "library"/"player"（含大写 alias "Library"/
        // "Player"），未知值 "nonsense" 解析失败 → 整份回退默认 + 备份。
        let dir = temp_cfg_dir("enum");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\nstartup_view = \"nonsense\"\n").unwrap();
        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.ui.startup_view, StartupView::Library);
        assert!(dir.join("config.toml.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_unreadable_path_uses_default_without_touching() {
        // config.toml 是目录 → 读取 IO 错误 → 默认值，原样不动
        let dir = temp_cfg_dir("unreadable");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&path).unwrap();
        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.ui.startup_view, StartupView::Library);
        assert_eq!(loaded.ui.side_panel_width, 240.0);
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_config_font_accent_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.ui.accent = 2;
        cfg.ui.font = FontChoice::System("微软雅黑".into());
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.accent, 2);
        assert_eq!(parsed.ui.font, FontChoice::System("微软雅黑".into()));
    }

    #[test]
    fn ui_config_font_system_toml_shape() {
        let text = r#"
[ui]
theme = "light"
font = { system = "黑体" }
accent = 1
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.font, FontChoice::System("黑体".into()));
        assert_eq!(parsed.ui.accent, 1);
    }

    #[test]
    fn danmaku_config_font_toml_shape_and_default() {
        let parsed: DanmakuConfig = toml::from_str(r#"font = "app""#).unwrap();
        assert_eq!(parsed.font, DanmakuFontChoice::App);
        let parsed: DanmakuConfig =
            toml::from_str(r#"font = { custom = "C:\\Fonts\\my.ttf" }"#).unwrap();
        assert_eq!(
            parsed.font,
            DanmakuFontChoice::Custom("C:\\Fonts\\my.ttf".into())
        );
        // 旧版本 [danmaku] 无 font 字段：serde(default) 回退 Default。
        let parsed: DanmakuConfig = toml::from_str("enabled = false").unwrap();
        assert_eq!(parsed.font, DanmakuFontChoice::Default);
        // 旧版本 [danmaku] 无 show_top/show_bottom 字段（2026-10 新增）：
        // serde(default) 回退 true（默认显示，不静默吞顶/底弹幕）。
        let parsed: DanmakuConfig = toml::from_str("enabled = false").unwrap();
        assert!(parsed.show_top);
        assert!(parsed.show_bottom);
        // 已移除的选项（yahei/simhei）：#[serde(other)] 回退 Default，
        // 不因未知值导致整份 config.toml 加载失败。
        for legacy in ["yahei", "simhei"] {
            let parsed: DanmakuConfig = toml::from_str(&format!(r#"font = "{legacy}""#)).unwrap();
            assert_eq!(parsed.font, DanmakuFontChoice::Default);
        }
        // roundtrip：单元变体为字符串；Custom 为 `[font] custom = '...'` 表
        // （toml 对 struct 内单键 map enum 的标准提升形状）。
        let text = toml::to_string(&DanmakuConfig::default()).unwrap();
        assert!(
            text.contains(r#"font = "default""#),
            "expected font = \"default\" in {text}"
        );
        let cfg = DanmakuConfig {
            font: DanmakuFontChoice::Custom("C:\\Fonts\\my.ttf".into()),
            ..Default::default()
        };
        let text = toml::to_string(&cfg).unwrap();
        let parsed: DanmakuConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.font, cfg.font);
    }

    #[test]
    fn ui_config_missing_font_accent_falls_back() {
        // 旧版本配置没有 font/accent 字段，serde(default) 应回退
        let text = r#"
[ui]
theme = "dark"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.ui.font, FontChoice::App);
        // 旧版本配置无 accent 字段时回退到新默认（紫，索引 3）。
        assert_eq!(parsed.ui.accent, 3);
    }

    #[test]
    fn save_atomic_creates_parent_and_roundtrips() {
        // 首次保存到不存在目录：create_dir_all + 原子 rename 后文件可完整解析。
        let dir = std::env::temp_dir().join(format!(
            "dyplayer-cfg-f1-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("config.toml");

        let mut cfg = AppConfig::default();
        cfg.player.show_stats_hud = true;
        cfg.save(&path).unwrap();

        assert!(path.exists(), "config file should exist after save");
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert!(parsed.player.show_stats_hud);

        // 同目录无 .config.toml.tmp 拖留。
        let tmp = dir.join("nested").join(".config.toml.tmp");
        assert!(!tmp.exists(), "temp file should not linger: {}", tmp.display());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_overwrite_replaces_content() {
        // 覆盖保存：旧值被新值完整替换，无半截文件。
        let dir = std::env::temp_dir().join(format!(
            "dyplayer-cfg-f1-overwrite-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut cfg = AppConfig::default();
        cfg.player.show_stats_hud = false;
        cfg.save(&path).unwrap();

        cfg.player.show_stats_hud = true;
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert!(parsed.player.show_stats_hud, "overwrite should take effect");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_no_truncation_on_rename_failure() {
        // rename 失败时（目标为目录）原文件不被截断、临时文件被清理。
        let dir = std::env::temp_dir().join(format!(
            "dyplayer-cfg-f1-renamefail-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 让 save 的 rename 目标命中一个已存在的目录：把 config.toml
        // 本身设为目录，rename(tmp, dir) 必失败。
        let path = dir.join("config.toml");
        std::fs::create_dir(&path).unwrap();

        let cfg = AppConfig::default();
        let err = cfg.save(&path);
        assert!(err.is_err(), "save to a directory path should fail");

        // 临时文件应被清理。
        let tmp = dir.join(".config.toml.tmp");
        assert!(!tmp.exists(), "temp file should be cleaned on rename failure");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
