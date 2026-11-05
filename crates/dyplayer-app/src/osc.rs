//! 播放页 OSC：ModernZ 风格自绘控制层。
//!
//! 批 2 起与 PlayerSession 解耦：绘制函数只依赖 [`OscState`]（主线程每帧
//! 组装的只读快照）+ [`OscUiState`]（显隐状态机/seek 预览/菜单开关等交互
//! 瞬态）+ [`OscCmd`] 命令队列——**一套代码两处运行**：SW 路径在主线程
//! 画（命令本地执行），GPU wid 路径在弹幕覆盖层线程画（悬浮于视频之上，
//! 命令经 OverlayBridge 回主线程执行）。
//!
//! 布局（早期验证页定稿）：
//! - 左组：媒体库(胶囊) / 媒体信息 / 截图 / 循环 / 音量
//! - 中组：上一集 / 快退 / 播放暂停 / 快进 / 下一集
//! - 右组：倍速 / 弹幕 / 音轨 / 字幕 / 播放列表 / 设置 / 全屏
//!
//! 显隐：全屏与非全屏统一启用，3s 空闲淡出；
//! 指针位于底部/顶部条带、弹层或面板打开、seek 拖拽中、暂停时保持显示。

use super::*;
use crate::glyph::{fade, mix, paint_left_center_truncated, truncate_tail};
use crate::osc_state::{OscCmd, OscLayout, OscNibble, OscState, OscUiState, TrackMenuKind};
use dyplayer_mpv::Track;
use egui::{
    Align2, Color32, CornerRadius, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2,
    vec2,
};
use std::time::{Duration, Instant};

/// OSC 无操作自动隐藏延迟。
const OSC_HIDE_AFTER: Duration = Duration::from_secs(3);
/// 淡入淡出时长（fadeduration=200ms）。
const OSC_FADE_TIME: f32 = 0.2;

/// 倍速档位（左键循环 / 右键复位 1x）。
const SPEEDS: [f64; 6] = [1.0, 1.25, 1.5, 2.0, 0.75, 0.5];
/// OSC 统一字体 family（HarmonyOS Sans SC，install_cjk_font 注册）。
const OSC_FAMILY: &str = "HarmonyOS Sans SC";

// ---------------------------------------------------------------------------
// 共享资产：图标码点 / OSC 主题 / SVG 图标；
// osc.rs / player_app / danmaku_ui / main 空态按钮共用）
// ---------------------------------------------------------------------------

/// 图标码点（modernz-icons.ttf PUA，fluent 系；OSC 按钮 / 队列行 / 菜单共享）。
pub(crate) mod icons {
    pub const PLAY: char = '\u{F065}';
    pub const PAUSE: char = '\u{F06D}';
    pub const SKIP_PREV: char = '\u{F061}';
    pub const SKIP_NEXT: char = '\u{F06F}';
    pub const REPLAY_10: char = '\u{F059}';
    pub const FORWARD_10: char = '\u{F051}';
    pub const AUDIO: char = '\u{F07C}';
    pub const SUBTITLE: char = '\u{F042}';
    pub const PLAYLIST: char = '\u{F064}';
    pub const VOLUME_MUTE: char = '\u{F046}';
    pub const VOLUME_LOW: char = '\u{F04C}';
    pub const VOLUME_HIGH: char = '\u{F048}';
    pub const LOOP_ON: char = '\u{F084}';
    pub const LOOP_OFF: char = '\u{F082}';
    pub const SCREENSHOT: char = '\u{F07A}';
    pub const INFO: char = '\u{F074}';
    pub const FULLSCREEN: char = '\u{F08A}';
    pub const FULLSCREEN_EXIT: char = '\u{F088}';
    /// 窗口控制钮码点（当前 titlebar 为自绘三钮，未消费；码点已验证，
    /// 保留在已用清单内供 icon_catalog 排除与后续查询）。
    #[allow(dead_code)]
    pub const WIN_CLOSE: char = '\u{F03A}';
    #[allow(dead_code)]
    pub const WIN_MIN: char = '\u{F038}';
    #[allow(dead_code)]
    pub const WIN_MAX: char = '\u{F039}';
    pub const SETTINGS: char = '\u{F072}';
}

/// OSC 主题（颜色选项；播放页 OSC / danmaku_ui 弹窗 / 设置页共享）。
#[derive(Clone, Copy)]
pub(crate) struct OscTheme {
    pub name: &'static str,
    /// seekbarfg_color / hover_effect_color / nibble_color。
    pub accent: Color32,
    /// seek_handle_color（handle 内圆填充）。
    pub handle_fill: Color32,
    /// seekbarbg_color。
    pub seekbar_bg: Color32,
    /// seekbar_cache_color。
    pub cache: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    /// held_element_color。
    pub held: Color32,
    /// nibble_current_color。
    pub nibble_current: Color32,
}

pub(crate) const THEMES: [OscTheme; 4] = [
    OscTheme {
        name: "紫（默认）",
        accent: Color32::from_rgb(0xB4, 0x8E, 0xF0),
        handle_fill: Color32::from_rgb(0x82, 0x57, 0xC4),
        seekbar_bg: Color32::from_rgb(0x99, 0x99, 0x99),
        cache: Color32::from_rgb(0xB1, 0xB1, 0xB1),
        text: Color32::WHITE,
        text_dim: Color32::from_rgb(0xC8, 0xC8, 0xC8),
        held: Color32::from_rgb(0x99, 0x99, 0x99),
        nibble_current: Color32::WHITE,
    },
    OscTheme {
        name: "蓝",
        accent: Color32::from_rgb(0x4A, 0x9F, 0xE8),
        handle_fill: Color32::from_rgb(0x2F, 0x6F, 0xAF),
        seekbar_bg: Color32::from_rgb(0x99, 0x99, 0x99),
        cache: Color32::from_rgb(0xB1, 0xB1, 0xB1),
        text: Color32::WHITE,
        text_dim: Color32::from_rgb(0xC8, 0xC8, 0xC8),
        held: Color32::from_rgb(0x99, 0x99, 0x99),
        nibble_current: Color32::WHITE,
    },
    OscTheme {
        name: "绿",
        accent: Color32::from_rgb(0x43, 0xCB, 0x44),
        handle_fill: Color32::from_rgb(0x2E, 0x9E, 0x3F),
        seekbar_bg: Color32::from_rgb(0x99, 0x99, 0x99),
        cache: Color32::from_rgb(0xB1, 0xB1, 0xB1),
        text: Color32::WHITE,
        text_dim: Color32::from_rgb(0xC8, 0xC8, 0xC8),
        held: Color32::from_rgb(0x99, 0x99, 0x99),
        nibble_current: Color32::WHITE,
    },
    OscTheme {
        name: "橙",
        accent: Color32::from_rgb(0xFF, 0x82, 0x32),
        handle_fill: Color32::from_rgb(0xC9, 0x65, 0x08),
        seekbar_bg: Color32::from_rgb(0x99, 0x99, 0x99),
        cache: Color32::from_rgb(0xB1, 0xB1, 0xB1),
        text: Color32::WHITE,
        text_dim: Color32::from_rgb(0xC8, 0xC8, 0xC8),
        held: Color32::from_rgb(0x99, 0x99, 0x99),
        nibble_current: Color32::WHITE,
    },
];

/// 注册 SVG 图标源并安装图像 loader（幂等，仅首次生效）。
/// 编译期内嵌：`include_bytes!` + `bytes://` URI +
/// egui_extras svg loader 按显示尺寸 × pixels_per_point 光栅化。
pub(crate) fn install_svg_icons(ctx: &egui::Context) {
    // 幂等（无 Once）：install_image_loaders 内置 is_loader_installed 判断，
    // include_bytes 同 URI 覆盖——多 ctx 场景（主线程 + GPU 弹幕覆盖层）
    // 各自装一次。**勿加进程级 Once**：首个调用者（SW 主线程/GPU 覆盖层）
    // 会独占初始化，另一个 ctx 的 SVG loader 永远装不上（GPU 下 OSC 媒体库
    // 胶囊 house 图标不显示的根因）。
    ctx.include_bytes(
        "bytes://dyplayer-icon-house.svg",
        include_bytes!("../assets/icons/house.svg"),
    );
    ctx.include_bytes(
        "bytes://dyplayer-icon-link.svg",
        include_bytes!("../assets/icons/link.svg"),
    );
    ctx.include_bytes(
        "bytes://dyplayer-app-icon.svg",
        include_bytes!("../assets/icons/dy-icon.svg"),
    );
    egui_extras::install_image_loaders(ctx);
}

/// 媒体库（首页）图标：白色 SVG × tint = 主题色，不触发重光栅化。
pub(crate) fn house_svg_image(color: Color32, size: f32) -> egui::Image<'static> {
    egui::Image::new("bytes://dyplayer-icon-house.svg")
        .fit_to_exact_size(vec2(size, size))
        .tint(color)
}

/// 串流链接图标（link.svg）。
pub(crate) fn link_svg_image(color: Color32, size: f32) -> egui::Image<'static> {
    egui::Image::new("bytes://dyplayer-icon-link.svg")
        .fit_to_exact_size(vec2(size, size))
        .tint(color)
}

/// hover 特效开关（设置页可独立控制；box 背景效果已按用户反馈移除）。
struct HoverFx {
    glow: bool,
    size: bool,
    color: bool,
}

fn hover_fx(st: &OscState) -> HoverFx {
    HoverFx {
        glow: st.fx_glow,
        size: st.fx_size,
        color: st.fx_color,
    }
}

/// OSC 底部渐变高度（Default 覆盖标题/进度/按钮三行；Mini 仅一行）。
fn osc_fade_h(st: &OscState) -> f32 {
    match st.layout {
        OscLayout::Default => 176.0,
        OscLayout::Mini => 70.0,
    }
}

pub(crate) fn osc_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(OSC_FAMILY.into()))
}


/// 鼠标活动检测：移动 / 点击重置空闲计时。
fn sense_mouse(osc: &mut OscUiState, ctx: &egui::Context) {
    let mp = ctx.input(|i| i.pointer.latest_pos());
    if mp != osc.last_mouse {
        osc.last_mouse = mp;
        osc.last_activity = Instant::now();
        osc.shown = true;
    }
    if ctx.input(|i| i.pointer.any_click() || i.pointer.any_down()) {
        osc.last_activity = Instant::now();
    }
}

/// OSC 主入口：显隐状态机 + 底部渐变 + 控制层 + 信息块。
/// 一套代码两处运行（主线程 SW / 覆盖线程 GPU，见模块文档）。
pub(crate) fn draw(
    st: &OscState,
    osc: &mut OscUiState,
    ui: &mut Ui,
    cmds: &mut Vec<OscCmd>,
) {
    install_svg_icons(ui.ctx());
    sense_mouse(osc, ui.ctx());

    let theme = THEMES[st.theme_idx.min(THEMES.len() - 1)];
    let rect = ui.max_rect();
    let ctx = ui.ctx().clone();
    let fade_h = osc_fade_h(st);

    // ---- 显隐状态机（空闲超时隐藏；指针在条带 / 轨道菜单 / 面板 / 拖拽 /
    //      暂停时保持显示）。----
    let mp = ctx.input(|i| i.pointer.latest_pos());
    let in_bottom = mp.is_some_and(|p| p.y >= rect.bottom() - fade_h - 24.0 && rect.contains(p));
    let in_top = mp.is_some_and(|p| p.y <= rect.top() + 46.0 && rect.contains(p));
    let menu_open = osc.track_menu.is_some();
    let panel_open = st.panel_open;
    let idle = osc.last_activity.elapsed() >= OSC_HIDE_AFTER;
    let keep = in_bottom || in_top || menu_open || panel_open || osc.seek_preview.is_some();
    osc.shown = !idle || keep || st.paused;

    let alpha = ctx.animate_value_with_time(
        Id::unique("player-osc-fade"),
        osc.shown as u8 as f32,
        OSC_FADE_TIME,
    );

    // 全屏点击层（画布语义，SW/GPU 统一在此处理）：双击 = 播放/暂停。
    // 单击唤醒已由 sense_mouse 的 any_click 覆盖；层在 OSC 控件之前
    // allocate——控件后注册赢 click（AGENTS.md 同层规则），空置区落到
    // 本层。track_menu 的外部关闭层在 Foreground 更高层优先。
    {
        let resp = ui.allocate_rect(rect, egui::Sense::click());
        if resp.double_clicked() {
            cmds.push(OscCmd::TogglePause);
            osc.last_activity = Instant::now();
        }
    }

    if alpha > 0.005 {
        // ---- 底部渐变背景。----
        // ~10px 的肉眼可见条带（层间 alpha 阶梯 0.05）。
        let grad = Rect::from_min_max(
            pos2(rect.left(), rect.bottom() - fade_h),
            pos2(rect.right(), rect.bottom()),
        );
        ui.painter().add(egui::Shape::gradient_rect(
            grad,
            egui::Direction::TopDown,
            [
                fade(Color32::BLACK, 0.0),
                fade(Color32::BLACK, 0.92 * alpha),
            ],
        ));
        match st.layout {
            OscLayout::Default => draw_controls(st, osc, ui, theme, alpha, cmds),
            OscLayout::Mini => draw_controls_mini(st, osc, ui, theme, alpha, cmds),
        }
    }

    draw_info_block(st, osc, ui, theme);
}

/// 控制层主体：标题 / seekbar / 三组按钮 / 音轨字幕 popup。
fn draw_controls(
    st: &OscState,
    osc: &mut OscUiState,
    ui: &mut Ui,
    theme: OscTheme,
    alpha: f32,
    cmds: &mut Vec<OscCmd>,
) {
    let rect = ui.max_rect();
    let p = ui.painter().clone();
    let x0 = rect.left() + 32.0;
    let x1 = rect.right() - 32.0;
    let bottom = rect.bottom();

    // 标题 + 章节标题（title / chapter_title）。
    p.text(
        pos2(x0, bottom - 150.0),
        Align2::LEFT_CENTER,
        truncate_tail(&st.title, 72),
        osc_font(21.0),
        fade(theme.text, alpha),
    );
    let preview_t = osc.seek_preview.unwrap_or(st.position);
    let ch_txt = match chapter_idx(st, preview_t) {
        Some(i) => {
            let t = &st.chapters[i];
            if t.title.is_empty() {
                format!("◈ 章节 {}", i + 1)
            } else {
                format!("◈ {}", t.title)
            }
        }
        None => String::new(),
    };
    if !ch_txt.is_empty() {
        p.text(
            pos2(x0, bottom - 126.0),
            Align2::LEFT_CENTER,
            ch_txt,
            osc_font(13.0),
            fade(theme.text_dim, alpha),
        );
    }

    // seekbar + 时间。
    draw_seekbar(st, osc, ui, theme, alpha, x0, x1, bottom - 100.0, 4.0, 7.0, cmds);
    let cur = osc.seek_preview.unwrap_or(st.position);
    p.text(
        pos2(x0, bottom - 76.0),
        Align2::LEFT_CENTER,
        format_time(cur),
        osc_font(13.5),
        fade(theme.text, alpha),
    );
    p.text(
        pos2(x1, bottom - 76.0),
        Align2::RIGHT_CENTER,
        format!("-{}", format_time((st.duration - cur).max(0.0))),
        osc_font(13.5),
        fade(theme.text_dim, alpha),
    );

    // ---- 按钮行 ----
    let cy = bottom - 40.0;

    // 左组：媒体库 / 媒体信息 / 截图 / 循环 / 音量。
    library_button(
        ui,
        theme,
        alpha,
        Rect::from_min_size(pos2(x0, cy - 15.0), vec2(96.0, 30.0)),
        cmds,
    );
    let loop_glyph = if st.loop_file {
        icons::LOOP_ON
    } else {
        icons::LOOP_OFF
    };
    let rw = 30.0;
    let rgap = 4.0;
    let mut lx = x0 + 96.0 + 12.0;
    let left: [(char, &str); 3] = [
        (icons::INFO, "媒体信息"),
        (icons::SCREENSHOT, "截图"),
        (loop_glyph, "循环播放"),
    ];
    for (i, (glyph, tip)) in left.iter().enumerate() {
        let center = pos2(lx + rw / 2.0, cy);
        let resp = icon_button_at(
            ui,
            Id::unique("player-osc-left").with(i),
            *glyph,
            21.0,
            center,
            theme,
            alpha,
            true,
            hover_fx(st),
            tip,
        );
        if resp.clicked() {
            match i {
                0 => cmds.push(OscCmd::ToggleHud),
                1 => cmds.push(OscCmd::TakeScreenshot),
                _ => cmds.push(OscCmd::ToggleLoopFile),
            }
        }
        lx += rw + rgap;
    }
    volume_control(st, ui, theme, alpha, pos2(lx + 8.0, cy), cmds);

    // 中组：上一集 / 快退 / 播放暂停 / 快进 / 下一集（边界集禁用置灰）。
    let play_glyph = if st.paused {
        icons::PLAY
    } else {
        icons::PAUSE
    };
    let step_back = st.seek_step_back;
    let step_fwd = st.seek_step_fwd;
    let can_prev = st.can_prev;
    let can_next = st.can_next;
    let mid: [(char, f32); 5] = [
        (icons::SKIP_PREV, 23.0),
        (icons::REPLAY_10, 23.0),
        (play_glyph, 30.0),
        (icons::FORWARD_10, 23.0),
        (icons::SKIP_NEXT, 23.0),
    ];
    let gap = 10.0;
    let widths: Vec<f32> = mid.iter().map(|it| it.1 + 10.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + gap * (mid.len() - 1) as f32;
    let mut x = rect.center().x - total / 2.0;
    for (i, (glyph, sz)) in mid.iter().enumerate() {
        let center = pos2(x + widths[i] / 2.0, cy);
        let (enabled, tip): (bool, String) = match i {
            0 => (
                can_prev,
                if can_prev {
                    "上一集".into()
                } else {
                    "已是第一集".into()
                },
            ),
            1 => (true, format!("后退 {step_back}s")),
            2 => (true, "播放 / 暂停".into()),
            3 => (true, format!("前进 {step_fwd}s")),
            _ => (
                can_next,
                if can_next {
                    "下一集".into()
                } else {
                    "已是最后一集".into()
                },
            ),
        };
        let resp = icon_button_at(
            ui,
            Id::unique("player-osc-mid").with(i),
            *glyph,
            *sz,
            center,
            theme,
            alpha,
            enabled,
            hover_fx(st),
            &tip,
        );
        if resp.clicked() && enabled {
            match i {
                0 => cmds.push(OscCmd::SkipPrev),
                1 => cmds.push(OscCmd::SeekRel(-step_back)),
                2 => cmds.push(OscCmd::TogglePause),
                3 => cmds.push(OscCmd::SeekRel(step_fwd)),
                _ => cmds.push(OscCmd::SkipNext),
            }
        }
        x += widths[i] + gap;
    }

    // 右组：倍速 / 弹幕 / 音轨 / 字幕 / 播放列表 / 设置 / 全屏（最右）。
    let fs_glyph = if st.fullscreen {
        icons::FULLSCREEN_EXIT
    } else {
        icons::FULLSCREEN
    };
    let fs_tip = if st.fullscreen {
        "退出全屏"
    } else {
        "全屏"
    };
    let speed_w = 40.0;
    let danmaku_w = 36.0;
    let tgap = 6.0;
    let n_right = 5usize;
    let icons_w = rw * n_right as f32 + rgap * (n_right - 1) as f32;
    let right_total = speed_w + danmaku_w + icons_w + tgap * 2.0;
    let mut x = x1 - right_total;
    speed_button(st, ui, theme, alpha, pos2(x + speed_w / 2.0, cy), cmds);
    x += speed_w + tgap;
    danmaku_button(st, osc, ui, theme, alpha, pos2(x + danmaku_w / 2.0, cy), cmds);
    x += danmaku_w + tgap;
    let right: [(char, &str); 5] = [
        (icons::AUDIO, "音轨"),
        (icons::SUBTITLE, "字幕"),
        (icons::PLAYLIST, "播放列表"),
        (icons::SETTINGS, "设置"),
        (fs_glyph, fs_tip),
    ];
    let mut audio_resp = None;
    let mut sub_resp = None;
    for (i, (glyph, tip)) in right.iter().enumerate() {
        let center = pos2(x + rw / 2.0, cy);
        let resp = icon_button_at(
            ui,
            Id::unique("player-osc-right").with(i),
            *glyph,
            21.0,
            center,
            theme,
            alpha,
            true,
            hover_fx(st),
            tip,
        );
        match i {
            0 => audio_resp = Some(resp.clone()),
            1 => sub_resp = Some(resp.clone()),
            2 => osc.queue_btn_rect = Some(resp.rect),
            3 => osc.settings_btn_rect = Some(resp.rect),
            _ => {}
        }
        if resp.clicked() {
            match i {
                // 音轨/字幕菜单：toggle（再点同按钮关闭）。
                0 => {
                    osc.track_menu = if osc.track_menu == Some(TrackMenuKind::Audio) {
                        None
                    } else {
                        Some(TrackMenuKind::Audio)
                    }
                }
                1 => {
                    osc.track_menu = if osc.track_menu == Some(TrackMenuKind::Subtitle) {
                        None
                    } else {
                        Some(TrackMenuKind::Subtitle)
                    }
                }
                2 => cmds.push(OscCmd::TogglePanelQueue),
                3 => cmds.push(OscCmd::TogglePanelSettings),
                4 => cmds.push(OscCmd::ToggleFullscreen),
                _ => {}
            }
        }
        x += rw + rgap;
    }
    let audio_resp = audio_resp.expect("audio button");
    let sub_resp = sub_resp.expect("subtitle button");

    // 音轨/字幕菜单（Foreground 定制卡片，弹幕设置弹窗同族工艺；锚定
    // 按钮上方，选择轨道 / 点击外部 / ESC / ✕ 关闭）。
    if osc.track_menu == Some(TrackMenuKind::Audio) {
        let anchor = pos2(audio_resp.rect.center().x, audio_resp.rect.top() - 6.0);
        track_menu_card(st, osc, ui, TrackMenuKind::Audio, anchor, theme, cmds);
    }
    if osc.track_menu == Some(TrackMenuKind::Subtitle) {
        let anchor = pos2(sub_resp.rect.center().x, sub_resp.rect.top() - 6.0);
        track_menu_card(st, osc, ui, TrackMenuKind::Subtitle, anchor, theme, cmds);
    }

    // 播放队列卡片（弹幕设置/轨道菜单同族工艺；锚定"播放列表"按钮上方）。
    if st.queue_open {
        let anchor = osc
            .queue_btn_rect
            .map(|r| pos2(r.center().x, r.top() - 6.0));
        queue_card(st, ui, anchor, theme, cmds);
    }

    // 设置卡片（同族工艺；锚定"设置"按钮上方）。
    if st.settings_open {
        let anchor = osc
            .settings_btn_rect
            .map(|r| pos2(r.center().x, r.top() - 6.0));
        settings_card(st, ui, anchor, theme, cmds);
    }
}

/// 音轨/字幕菜单卡片（弹幕设置弹窗同族工艺：Foreground 层 + 深色半透明
/// 卡片 + osc_font + OscTheme；自绘单选行 + 外部文件加载入口）。
/// 锚定对应按钮上方；选择轨道 / 点击外部 / ESC / ✕ 关闭。
/// rfd 文件对话框经 OscCmd 移交主线程（覆盖线程不能弹）。
fn track_menu_card(
    st: &OscState,
    osc: &mut OscUiState,
    ui: &mut Ui,
    kind: TrackMenuKind,
    anchor: Pos2,
    theme: OscTheme,
    cmds: &mut Vec<OscCmd>,
) {
    use crate::danmaku_ui::{settings_card_bg, settings_card_outline, settings_card_rect};

    let screen = ui.max_rect();
    let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("osc-track-menu"));
    let mut lui = ui.new_child(
        egui::UiBuilder::new()
            .layer_id(layer)
            .max_rect(screen)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );

    // 全屏透明命中层：点击外部（主/右键）或 ESC 关闭。
    let outside = lui.interact(screen, egui::Id::unique("osc-track-menu-outside"), Sense::click());
    let close_now = outside.clicked()
        || outside.secondary_clicked()
        || lui.ctx().input(|i| i.key_pressed(egui::Key::Escape));

    let (title, tracks, off_label) = match kind {
        TrackMenuKind::Audio => ("音轨", &st.audio_tracks, "无音轨"),
        TrackMenuKind::Subtitle => ("字幕", &st.sub_tracks, "无字幕"),
    };

    // 卡片高度自适应轨道数；列表区超 9 行进滚动（容器内多字幕轨常见 10+）。
    const ROW_H: f32 = 30.0;
    let list_h = ((tracks.len() as f32 + 1.0) * ROW_H).min(ROW_H * 9.0);
    let size = vec2(280.0, 18.0 + 26.0 + 6.0 + list_h + 7.0 + 1.0 + 7.0 + 30.0 + 18.0);
    let card = settings_card_rect(screen, Some(anchor), size);
    let p = lui.painter();
    p.rect_filled(card, CornerRadius::same(14), settings_card_bg());
    p.rect_stroke(
        card.shrink(0.5),
        CornerRadius::same(14),
        Stroke::new(1.0, settings_card_outline()),
        StrokeKind::Inside,
    );
    let _ = lui.interact(card, egui::Id::unique("osc-track-menu-card"), Sense::click());

    let mut mui = lui.new_child(
        egui::UiBuilder::new()
            .id_salt("osc-track-menu-body")
            .max_rect(card.shrink2(vec2(22.0, 18.0)))
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    mui.spacing_mut().item_spacing.y = 0.0;

    // 标题行 + 关闭 ✕（from_min_size 收边：中心贴右界会溢出内容区）。
    let mut close_x = false;
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 26.0), Sense::hover());
        let p = mui.painter();
        p.text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            title,
            osc_font(15.0),
            theme.text,
        );
        let x_rect = Rect::from_min_size(
            pos2(rect.right() - 24.0, rect.center().y - 12.0),
            vec2(24.0, 24.0),
        );
        let resp = mui.interact(x_rect, egui::Id::unique("osc-track-menu-close"), Sense::click());
        let t = mui.ctx().animate_value_with_time(
            egui::Id::unique("osc-track-menu-close-h"),
            resp.hovered() as u8 as f32,
            0.08,
        );
        if t > 0.01 {
            p.circle_filled(x_rect.center(), 11.0, fade(Color32::WHITE, 0.10 * t));
        }
        let c = mix(theme.text_dim, theme.text, t);
        let k = 3.4 + 0.4 * t;
        p.line_segment(
            [
                pos2(x_rect.center().x - k, x_rect.center().y - k),
                pos2(x_rect.center().x + k, x_rect.center().y + k),
            ],
            Stroke::new(1.4, c),
        );
        p.line_segment(
            [
                pos2(x_rect.center().x - k, x_rect.center().y + k),
                pos2(x_rect.center().x + k, x_rect.center().y - k),
            ],
            Stroke::new(1.4, c),
        );
        if resp.clicked() {
            close_x = true;
        }
    }

    mui.add_space(6.0);
    let mut picked: Option<Option<i64>> = None;
    egui::ScrollArea::vertical()
        .id_salt(egui::Id::unique("osc-track-menu-list"))
        .max_height(list_h)
        .show_viewport(&mut mui, |ui, _| {
            let any_selected = tracks.iter().any(|t| t.selected);
            if track_row(ui, "osc-track-off", off_label, !any_selected, theme).clicked() {
                picked = Some(None);
            }
            for t in tracks.iter() {
                if track_row(ui, "osc-track-item", &track_label(t), t.selected, theme).clicked() {
                    picked = Some(Some(t.id));
                }
            }
        });

    mui.add_space(7.0);
    let (hr, _) = mui.allocate_exact_size(vec2(mui.available_width(), 1.0), Sense::hover());
    mui.painter()
        .rect_filled(hr, CornerRadius::ZERO, fade(Color32::WHITE, 0.10));
    mui.add_space(7.0);

    // 底行：加载外部音频/字幕（描边胶囊，hover 转强调）。
    let mut pick_clicked = false;
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 30.0), Sense::hover());
        let p = mui.painter();
        let label = match kind {
            TrackMenuKind::Audio => "加载外部音频…",
            TrackMenuKind::Subtitle => "加载外部字幕…",
        };
        let gal = p.layout_no_wrap(label.to_owned(), osc_font(12.0), theme.text);
        let pw = gal.size().x + 22.0;
        let pill = Rect::from_min_max(
            pos2(rect.right() - pw, rect.center().y - 13.0),
            pos2(rect.right(), rect.center().y + 13.0),
        );
        let resp =
            mui.interact(pill, egui::Id::unique("osc-track-menu-pick"), Sense::click());
        let t = mui.ctx().animate_value_with_time(
            egui::Id::unique("osc-track-menu-pick-h"),
            resp.hovered() as u8 as f32,
            0.09,
        );
        p.rect_stroke(
            pill,
            CornerRadius::same(13),
            Stroke::new(1.0, mix(fade(Color32::WHITE, 0.30), theme.accent, t)),
            StrokeKind::Inside,
        );
        if t > 0.01 {
            p.rect_filled(pill, CornerRadius::same(13), fade(theme.accent, 0.12 * t));
        }
        p.galley(
            pos2(pill.left() + 11.0, pill.center().y - gal.size().y / 2.0),
            gal,
            theme.text,
        );
        if resp.clicked() {
            pick_clicked = true;
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }

    if let Some(id) = picked {
        match kind {
            TrackMenuKind::Audio => cmds.push(OscCmd::SelectAudio(id)),
            TrackMenuKind::Subtitle => cmds.push(OscCmd::SelectSub(id)),
        }
        osc.track_menu = None;
    }
    if close_now || close_x {
        osc.track_menu = None;
    }
    if pick_clicked {
        match kind {
            TrackMenuKind::Audio => cmds.push(OscCmd::PickAudioFile),
            TrackMenuKind::Subtitle => cmds.push(OscCmd::PickSubFile),
        }
    }
}

/// 轨道单选行：✓ + 标签；当前轨 accent 高亮，hover 微亮底（整行可点）。
fn track_row(ui: &mut Ui, id: &str, label: &str, selected: bool, theme: OscTheme) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let p = ui.painter();
    let t = ui.ctx().animate_value_with_time(
        egui::Id::unique(id).with(label),
        resp.hovered() as u8 as f32,
        0.08,
    );
    if t > 0.01 {
        p.rect_filled(rect, CornerRadius::same(6), fade(Color32::WHITE, 0.06 * t));
    }
    if selected {
        p.rect_filled(rect, CornerRadius::same(6), fade(theme.accent, 0.12));
    }
    let text_c = if selected {
        theme.accent
    } else {
        theme.text_dim
    };
    // ✓：自绘对勾（无字形依赖）。
    let c = pos2(rect.left() + 9.0, rect.center().y);
    if selected {
        p.line_segment(
            [pos2(c.x - 3.4, c.y + 0.4), pos2(c.x - 1.2, c.y + 2.6)],
            Stroke::new(1.7, theme.accent),
        );
        p.line_segment(
            [pos2(c.x - 1.2, c.y + 2.6), pos2(c.x + 3.8, c.y - 2.6)],
            Stroke::new(1.7, theme.accent),
        );
    }
    p.text(
        pos2(c.x + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        truncate_tail(label, 22),
        osc_font(12.5),
        text_c,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 媒体库按钮：胶囊形（半透明白描边 + house.svg 图标 + 文本），返回浏览界面。
fn library_button(ui: &mut Ui, theme: OscTheme, alpha: f32, rect: Rect, cmds: &mut Vec<OscCmd>) {

    let resp = response_at(ui, rect, Sense::click());
    let ctx = ui.ctx();
    let t = ctx.animate_value_with_time(
        Id::unique("player-osc-lib"),
        resp.hovered() as u8 as f32,
        0.08,
    );
    let radius = (rect.height() / 2.0) as u8;
    let p = ui.painter().clone();
    p.rect_filled(
        rect,
        CornerRadius::same(radius),
        fade(Color32::WHITE, (0.10 + 0.08 * t) * alpha),
    );
    p.rect_stroke(
        rect,
        CornerRadius::same(radius),
        Stroke::new(1.2, fade(Color32::WHITE, (0.50 + 0.25 * t) * alpha)),
        StrokeKind::Inside,
    );
    let icon_sz = 15.0;
    let gap = 6.0;
    let galley = p.layout_no_wrap("媒体库".to_owned(), osc_font(13.0), fade(theme.text, alpha));
    let content_w = icon_sz + gap + galley.size().x;
    let cx = rect.center().x - content_w / 2.0;
    let cy = rect.center().y;
    let _ = ui.put(
        Rect::from_center_size(pos2(cx + icon_sz / 2.0, cy), vec2(icon_sz, icon_sz)),
        house_svg_image(fade(theme.text, alpha), icon_sz),
    );
    p.galley(
        pos2(cx + icon_sz + gap, cy - galley.size().y / 2.0),
        galley,
        Color32::WHITE,
    );
    if resp.hovered() {
        paint_tooltip(
            ui,
            pos2(rect.center().x, rect.top() - 2.0),
            "返回媒体库",
            1.0,
        );
    }
    if resp.clicked() {
        cmds.push(OscCmd::ExitToLibrary);
    }
}

/// 图标按钮：固定槽位 + painter 绘制 glyph；hover 三效（glow/size/color）
/// 按设置页开关渲染，held 变灰；disabled 整体置灰且无动效。
#[allow(clippy::too_many_arguments)]
fn icon_button_at(
    ui: &mut Ui,
    id: Id,
    glyph: char,
    size: f32,
    center: Pos2,
    theme: OscTheme,
    alpha: f32,
    enabled: bool,
    fx: HoverFx,
    tooltip: &str,
) -> Response {
    let slot_side = size.max(18.0) + 12.0;
    let rect = Rect::from_center_size(center, vec2(slot_side, slot_side));
    let resp = response_at(ui, rect, Sense::click());

    let ctx = ui.ctx();
    let hovered = enabled && resp.hovered() && alpha > 0.3;
    let t = ctx.animate_value_with_time(id.with("h"), hovered as u8 as f32, 0.08);
    let held = enabled && resp.is_pointer_button_down_on();
    let eff_alpha = if enabled { alpha } else { alpha * 0.35 };

    let p = ui.painter();
    let base = if held { theme.held } else { theme.text };
    let color = if fx.color {
        mix(base, theme.accent, t)
    } else {
        base
    };
    let s = if fx.size {
        size * (1.0 + 0.16 * t)
    } else {
        size
    };

    if fx.glow && t > 0.01 {
        for (grow, ga) in [(1.55f32, 0.08f32), (1.30, 0.14), (1.14, 0.22)] {
            p.text(
                center,
                Align2::CENTER_CENTER,
                glyph,
                osc_font(s * grow),
                fade(theme.accent, ga * t * alpha),
            );
        }
    }
    p.text(
        center,
        Align2::CENTER_CENTER,
        glyph,
        osc_font(s),
        fade(color, eff_alpha),
    );
    if resp.hovered() && !tooltip.is_empty() {
        paint_tooltip(ui, pos2(center.x, rect.top() - 2.0), tooltip, 1.0);
    }
    resp
}

/// 倍速按钮（文字按钮，左键循环 / 右键复位 1x）。
fn speed_button(
    st: &OscState,
    ui: &mut Ui,
    theme: OscTheme,
    alpha: f32,
    center: Pos2,
    cmds: &mut Vec<OscCmd>,
) {
    let rect = Rect::from_center_size(center, vec2(40.0, 26.0));
    let resp = response_at(ui, rect, Sense::click());
    let ctx = ui.ctx();
    let t = ctx.animate_value_with_time(
        Id::unique("player-osc-speed"),
        resp.hovered() as u8 as f32,
        0.08,
    );
    let label = speed_label(st.speed);
    let color = if st.fx_color {
        mix(theme.text, theme.accent, t)
    } else {
        theme.text
    };
    let s = if st.fx_size {
        14.0 * (1.0 + 0.16 * t)
    } else {
        14.0
    };
    ui.painter().text(
        center,
        Align2::CENTER_CENTER,
        label,
        osc_font(s),
        fade(color, alpha),
    );
    if resp.clicked() {
        let next = SPEEDS
            .iter()
            .position(|&v| (v - st.speed).abs() < 1e-9)
            .map(|i| SPEEDS[(i + 1) % SPEEDS.len()])
            .unwrap_or(SPEEDS[1]);
        cmds.push(OscCmd::SetSpeed(next));
    } else if resp.secondary_clicked() {
        cmds.push(OscCmd::SetSpeed(1.0));
    }
    if resp.hovered() {
        paint_tooltip(
            ui,
            pos2(center.x, rect.top() - 2.0),
            "倍速（右键复位 1x）",
            1.0,
        );
    }
}

/// 弹幕文本按钮：左键切换弹幕显示，右键打开弹幕设置面板。
fn danmaku_button(
    st: &OscState,
    osc: &mut OscUiState,
    ui: &mut Ui,
    theme: OscTheme,
    alpha: f32,
    center: Pos2,
    cmds: &mut Vec<OscCmd>,
) {
    let rect = Rect::from_center_size(center, vec2(36.0, 26.0));
    let resp = response_at(ui, rect, Sense::click());
    let ctx = ui.ctx();
    let t = ctx.animate_value_with_time(
        Id::unique("player-osc-danmaku"),
        resp.hovered() as u8 as f32,
        0.08,
    );
    // 开启：默认白，hover → accent；关闭：暗灰，hover 提亮。
    let (base, target) = if st.danmaku_enabled {
        (theme.text, theme.accent)
    } else {
        (theme.text_dim, theme.text)
    };
    let color = if st.fx_color {
        mix(base, target, t)
    } else {
        base
    };
    let s = if st.fx_size {
        14.0 * (1.0 + 0.16 * t)
    } else {
        14.0
    };
    ui.painter().text(
        center,
        Align2::CENTER_CENTER,
        "弹幕",
        osc_font(s),
        fade(color, alpha),
    );
    if resp.clicked() {
        cmds.push(OscCmd::ToggleDanmakuEnabled);
    } else if resp.secondary_clicked() {
        cmds.push(OscCmd::ToggleDanmakuSettings);
    }
    if resp.hovered() {
        paint_tooltip(
            ui,
            pos2(center.x, rect.top() - 2.0),
            "弹幕开关（右键设置）",
            1.0,
        );
    }
    osc.danmaku_btn_rect = Some(rect);
}

/// 音量控制：静音图标（0/低/高三态）+ 迷你音量条（0-100）。
fn volume_control(
    st: &OscState,
    ui: &mut Ui,
    theme: OscTheme,
    alpha: f32,
    center: Pos2,
    cmds: &mut Vec<OscCmd>,
) {
    let glyph_vol = if st.muted { 0 } else { st.volume };
    let glyph = if glyph_vol <= 0 {
        icons::VOLUME_MUTE
    } else if glyph_vol < 40 {
        icons::VOLUME_LOW
    } else {
        icons::VOLUME_HIGH
    };
    let tip = format!("静音（当前 {}%）", st.volume);
    let resp = icon_button_at(
        ui,
        Id::unique("player-osc-vol"),
        glyph,
        21.0,
        center,
        theme,
        alpha,
        true,
        hover_fx(st),
        &tip,
    );
    if resp.clicked() {
        cmds.push(OscCmd::ToggleMute);
    }

    let bar_x0 = center.x + 18.0;
    let bar_x1 = bar_x0 + 70.0;
    let hit = Rect::from_min_max(pos2(bar_x0, center.y - 9.0), pos2(bar_x1, center.y + 9.0));
    let vresp = response_at(ui, hit, Sense::drag());
    if let Some(pt) = vresp.interact_pointer_pos() {
        let v = ((pt.x - bar_x0) / (bar_x1 - bar_x0)).clamp(0.0, 1.0) * 100.0;
        // SetVolume 主线程实现含 unmute 语义（原 sess.set_volume + unmute）。
        cmds.push(OscCmd::SetVolume(v as i64));
    }
    let shown = if st.muted {
        0.0
    } else {
        st.volume as f32 / 100.0
    };
    let p = ui.painter();
    let track = Rect::from_min_max(pos2(bar_x0, center.y - 1.5), pos2(bar_x1, center.y + 1.5));
    p.rect_filled(
        track,
        CornerRadius::same(2),
        fade(theme.seekbar_bg, 0.55 * alpha),
    );
    let fill_to = bar_x0 + (bar_x1 - bar_x0) * shown;
    p.rect_filled(
        Rect::from_min_max(pos2(bar_x0, center.y - 1.5), pos2(fill_to, center.y + 1.5)),
        CornerRadius::same(2),
        fade(theme.accent, alpha),
    );
    if vresp.hovered() || vresp.is_pointer_button_down_on() {
        p.circle_filled(pos2(fill_to, center.y), 5.0, fade(Color32::WHITE, alpha));
    }
    if vresp.hovered() {
        paint_tooltip(
            ui,
            pos2((bar_x0 + bar_x1) / 2.0, center.y - 12.0),
            &format!("{:.0}%", shown * 100.0),
            1.0,
        );
    }
}

/// 自绘 seekbar：圆角轨道 / 缓冲范围（pos→缓冲前沿）/ 章节 Gap 断开 /
/// 双层圆形 handle / hover tooltip（时间+章节）/ 拖拽预览。
/// 交互与原生 Slider 同款：Sense::drag() 拖出区域仍持续跟踪。
#[allow(clippy::too_many_arguments)]
fn draw_seekbar(
    st: &OscState,
    osc: &mut OscUiState,
    ui: &mut Ui,
    theme: OscTheme,
    alpha: f32,
    x0: f32,
    x1: f32,
    cy: f32,
    track_h: f32,
    handle_r: f32,
    cmds: &mut Vec<OscCmd>,
) {
    let duration = st.duration;
    if duration <= 0.0 {
        return;
    }
    let ctx = ui.ctx().clone();
    let p = ui.painter().clone();
    let half = track_h / 2.0;
    let corner = half as u8;
    let track = Rect::from_min_max(pos2(x0, cy - half), pos2(x1, cy + half));
    let hit = track.expand(8.0);
    let resp = response_at(ui, hit, Sense::drag());

    // 按住 / 拖动 → 预览；松开 → seek。
    if let Some(pt) = resp.interact_pointer_pos() {
        let t = ((pt.x - x0) / (x1 - x0)).clamp(0.0, 1.0) as f64 * duration;
        osc.seek_preview = Some(t);
        osc.last_activity = Instant::now();
    }
    if !ctx.input(|i| i.pointer.any_down())
        && let Some(t) = osc.seek_preview.take()
    {
        cmds.push(OscCmd::SeekTo(t));
    }

    let media_time = st.position;
    let pos_t = osc.seek_preview.unwrap_or(media_time);
    let x_of = |t: f64| x0 + (t / duration).clamp(0.0, 1.0) as f32 * (x1 - x0);
    let pos_x = x_of(pos_t);

    // 1. 背景（seekbarbg）。
    p.rect_filled(
        track,
        CornerRadius::same(corner),
        fade(theme.seekbar_bg, 0.55 * alpha),
    );

    // 2. 缓冲范围（0 → 缓冲前沿，seekbar_cache）。
    let buf_end = (media_time + st.cache_time).min(duration);
    let buf_x = x_of(buf_end);
    if buf_x > x0 {
        p.rect_filled(
            Rect::from_min_max(pos2(x0, cy - half), pos2(buf_x, cy + half)),
            CornerRadius::same(corner),
            fade(theme.cache, 0.45 * alpha),
        );
    }

    // 3. 进度（seekbarfg；Gap 样式在章节处断开 3px，其余样式连续）。
    if st.nibble == OscNibble::Gap {
        let mut seg_start = x0;
        for ch in &st.chapters {
            let cx = x_of(ch.time);
            if cx <= x0 || cx >= pos_x {
                continue;
            }
            if cx - 1.5 > seg_start {
                p.rect_filled(
                    Rect::from_min_max(pos2(seg_start, cy - half), pos2(cx - 1.5, cy + half)),
                    CornerRadius::same(corner),
                    fade(theme.accent, alpha),
                );
            }
            seg_start = cx + 1.5;
        }
        if pos_x > seg_start {
            p.rect_filled(
                Rect::from_min_max(pos2(seg_start, cy - half), pos2(pos_x, cy + half)),
                CornerRadius::same(corner),
                fade(theme.accent, alpha),
            );
        }
    } else if pos_x > x0 {
        p.rect_filled(
            Rect::from_min_max(pos2(x0, cy - half), pos2(pos_x, cy + half)),
            CornerRadius::same(corner),
            fade(theme.accent, alpha),
        );
    }

    // 4. 章节标记（Bar 上下条 / Triangle 上三角；Gap 由进度断开表达）。
    if st.nibble != OscNibble::Gap {
        let cur_ch = chapter_idx(st, pos_t);
        for (i, ch) in st.chapters.iter().enumerate() {
            let cx = x_of(ch.time);
            if cx < x0 + 1.0 || cx > x1 - 1.0 {
                continue;
            }
            let c = if cur_ch == Some(i) {
                theme.nibble_current
            } else {
                theme.accent
            };
            match st.nibble {
                OscNibble::Bar => {
                    p.rect_filled(
                        Rect::from_center_size(pos2(cx, cy - half - 3.0), vec2(2.5, 4.0)),
                        CornerRadius::same(1),
                        fade(c, alpha),
                    );
                    p.rect_filled(
                        Rect::from_center_size(pos2(cx, cy + half + 3.0), vec2(2.5, 4.0)),
                        CornerRadius::same(1),
                        fade(c, alpha),
                    );
                }
                OscNibble::Triangle => {
                    let top_y = cy - half - 2.0;
                    p.add(egui::Shape::convex_polygon(
                        vec![
                            pos2(cx - 3.5, top_y),
                            pos2(cx + 3.5, top_y),
                            pos2(cx, top_y - 5.0),
                        ],
                        fade(c, alpha),
                        Stroke::NONE,
                    ));
                }
                OscNibble::Gap => unreachable!(),
            }
        }
    }

    // 4. handle：外圆边框色 + 内圆填充色；hover / 拖拽时边框变细。
    let near_handle = ctx
        .input(|i| i.pointer.latest_pos())
        .map(|pt| (pt.x - pos_x).abs() <= handle_r + 8.0)
        .unwrap_or(false);
    let handle_hover = near_handle || osc.seek_preview.is_some();
    let border_t = if handle_hover { 0.31 } else { 0.42 };
    let inner_r = handle_r * (1.0 - border_t);
    p.circle_filled(pos2(pos_x, cy), handle_r, fade(theme.accent, alpha));
    p.circle_filled(pos2(pos_x, cy), inner_r, fade(theme.handle_fill, alpha));

    // 5. tooltip：悬停时间 + 所在章节。
    let hover_pos = ctx.input(|i| i.pointer.interact_pos());
    let show_tip = (resp.hovered() && hover_pos.is_some()) || osc.seek_preview.is_some();
    if show_tip {
        let tip_t = osc
            .seek_preview
            .or_else(|| {
                hover_pos.map(|pt| ((pt.x - x0) / (x1 - x0)).clamp(0.0, 1.0) as f64 * duration)
            })
            .unwrap_or(pos_t);
        let label = match st.chapters.iter().rev().find(|ch| ch.time <= tip_t) {
            Some(ch) if !ch.title.is_empty() => format!("{} · {}", format_time(tip_t), ch.title),
            _ => format_time(tip_t),
        };
        let tip_x = x_of(tip_t).clamp(x0 + 36.0, x1 - 36.0);
        paint_tooltip(ui, pos2(tip_x, cy - handle_r - 5.0), &label, alpha);
    }
}

/// Mini 布局：播放按钮 + 迷你条 + 时间（验证页 draw_osc_mini 正式版）。
fn draw_controls_mini(
    st: &OscState,
    osc: &mut OscUiState,
    ui: &mut Ui,
    theme: OscTheme,
    alpha: f32,
    cmds: &mut Vec<OscCmd>,
) {
    let rect = ui.max_rect();
    let bottom = rect.bottom();
    let cy = bottom - 30.0;
    let margin = 44.0;

    let play_glyph = if st.paused {
        icons::PLAY
    } else {
        icons::PAUSE
    };
    let resp = icon_button_at(
        ui,
        Id::unique("player-osc-mini-play"),
        play_glyph,
        24.0,
        pos2(rect.left() + margin + 12.0, cy),
        theme,
        alpha,
        true,
        hover_fx(st),
        "播放 / 暂停",
    );
    if resp.clicked() {
        cmds.push(OscCmd::TogglePause);
    }

    let sx0 = rect.left() + margin + 42.0;
    let sx1 = rect.right() - margin - 120.0;
    draw_seekbar(st, osc, ui, theme, alpha, sx0, sx1, cy, 3.0, 5.5, cmds);

    // B02：Mini 布局保留恢复 Default 入口（设置卡片在 Default 分支，Mini
    // 下原无设置入口；点此切回 Default 即可再进设置切布局/特效）。
    let expand_resp = icon_button_at(
        ui,
        Id::unique("player-osc-mini-expand"),
        icons::SETTINGS,
        16.0,
        pos2(rect.right() - margin - 110.0, cy),
        theme,
        alpha,
        true,
        hover_fx(st),
        "展开控制栏",
    );
    if expand_resp.clicked() {
        cmds.push(OscCmd::SetOscPrefs {
            theme_idx: st.theme_idx,
            layout: OscLayout::Default,
            nibble: st.nibble,
            fx_glow: st.fx_glow,
            fx_size: st.fx_size,
            fx_color: st.fx_color,
        });
    }

    let cur = osc.seek_preview.unwrap_or(st.position);
    ui.painter().text(
        pos2(rect.right() - margin, cy),
        Align2::RIGHT_CENTER,
        format!("{} / {}", format_time(cur), format_time(st.duration)),
        osc_font(13.0),
        fade(theme.text, alpha),
    );
}

/// 媒体信息块（左下；受"显示媒体信息块"开关控制，不受 OSC 淡出影响）。
/// 合并原 HUD 底部统计条内容（解码/帧率/尺寸）与播放状态。
fn draw_info_block(st: &OscState, osc: &OscUiState, ui: &Ui, theme: OscTheme) {
    if !st.show_hud {
        return;
    }

    let rect = ui.max_rect();
    let pos_t = osc.seek_preview.unwrap_or(st.position);
    let chapter = match chapter_idx(st, pos_t) {
        Some(i) => {
            if st.chapters[i].title.is_empty() {
                format!("章节 {}", i + 1)
            } else {
                st.chapters[i].title.clone()
            }
        }
        None => "无".into(),
    };
    let vol = if st.muted {
        "静音".to_string()
    } else {
        format!("{}%", st.volume)
    };
    let size = st
        .video_size
        .map(|[w, h]| format!("{w}x{h}"))
        .unwrap_or_else(|| "N/A".into());
    let lines = [
        format!(
            "时间  {} / {}",
            format_time(pos_t),
            format_time(st.duration)
        ),
        format!("速度  {}", speed_label(st.speed)),
        format!("音量  {vol}"),
        format!("章节  {chapter}"),
        format!("缓冲  +{:.0}s", st.cache_time),
        format!(
            "解码  {}",
            format_decode_info(&st.codec, &st.hwdec, st.vo_drops)
        ),
        // 三种帧率分列，直观评估流畅度：
        // 视频 = mpv container-fps（媒体属性）；渲染 = egui UI 重绘率（画面
        // /OSC/弹幕共用管线，暂停时回落到 ~10 属正常）；弹幕 = 弹幕层有可见
        // 内容提交的帧率（开启且 dm_fps=0 表示当前无可见弹幕）。
        format!("视频  {}", fmt_fps(st.video_fps)),
        format!("渲染  {}", fmt_fps(st.ui_fps)),
        format!("弹幕  {}", danmaku_fps_label(st)),
        format!("尺寸  {size}"),
        format!("队列  {}/{}", st.queue_pos, st.queue_len),
        format!("状态  {}", truncate_tail(&st.status, 44)),
    ];
    let block_h = lines.len() as f32 * 17.0 + 12.0;
    let top = (rect.bottom() - osc_fade_h(st) - 16.0 - block_h).max(rect.top() + 8.0);
    let block = Rect::from_min_size(pos2(rect.left() + 20.0, top), vec2(240.0, block_h));
    let p = ui.painter();
    p.rect_filled(block, CornerRadius::same(6), fade(Color32::BLACK, 0.55));
    for (i, line) in lines.iter().enumerate() {
        p.text(
            pos2(block.left() + 10.0, block.top() + 14.0 + i as f32 * 17.0),
            Align2::LEFT_CENTER,
            line,
            osc_font(12.0),
            theme.text_dim,
        );
    }
}

/// 当前章节索引（时间 ≤ t 的最后一个章节；None = 未进入第一章）。
fn chapter_idx(st: &OscState, t: f64) -> Option<usize> {
    let mut idx = None;
    for (i, ch) in st.chapters.iter().enumerate() {
        if ch.time <= t + f64::EPSILON {
            idx = Some(i);
        }
    }
    idx
}

/// 轨道显示名：title 优先，补 lang；全空时回退"轨道 {id}"。
fn track_label(t: &Track) -> String {
    if !t.title.is_empty() {
        if !t.lang.is_empty() {
            format!("{} ({})", t.title, t.lang)
        } else {
            t.title.clone()
        }
    } else if !t.lang.is_empty() {
        t.lang.clone()
    } else {
        format!("轨道 {}", t.id)
    }
}

/// 在绝对位置放一个响应区域（不依赖布局流）。
fn response_at(ui: &mut Ui, rect: Rect, sense: Sense) -> Response {
    ui.allocate_rect(rect, sense)
}

/// 简易 tooltip：圆角黑底 + 白字，锚点上方。
fn paint_tooltip(ui: &Ui, anchor: Pos2, text: &str, alpha: f32) {
    let font = osc_font(12.5);
    let gal = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, Color32::WHITE);
    let w = gal.size().x + 14.0;
    let h = gal.size().y + 7.0;
    let rect = Rect::from_min_size(pos2(anchor.x - w / 2.0, anchor.y - h - 2.0), vec2(w, h));
    let clip = ui.clip_rect();
    let rect = rect.intersect(clip);
    if !rect.is_positive() {
        return;
    }
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(4),
        fade(Color32::from_rgb(0x14, 0x14, 0x14), 0.92 * alpha),
    );
    p.galley(rect.min + vec2(7.0, 3.5), gal, Color32::WHITE);
}


fn speed_label(s: f64) -> String {
    if (s - s.round()).abs() < 1e-9 {
        format!("{}x", s.round() as i64)
    } else {
        format!("{s}x")
    }
}

/// 帧率显示：去尾零（23.976 / 25 / 60）；未加载视频时 N/A。
fn fmt_fps(v: f64) -> String {
    if v <= 0.0 {
        "N/A".into()
    } else {
        format!("{:.3}", v)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// 弹幕帧率标签：关闭 → "关"；开启无可见弹幕 → "—"；否则数值。
fn danmaku_fps_label(st: &OscState) -> String {
    if !st.danmaku_enabled {
        "关".into()
    } else if st.dm_fps > 0.0 {
        fmt_fps(st.dm_fps)
    } else {
        "—".into()
    }
}

/// 播放队列卡片（弹幕设置/轨道菜单同族工艺：Foreground 层 + 深色半透明
/// 卡片 + osc_font + OscTheme；自绘列表行）。锚定"播放列表"按钮上方；
/// 行点击跳转 / 点击外部 / ESC / ✕ 关闭。
fn queue_card(
    st: &OscState,
    ui: &mut Ui,
    anchor: Option<Pos2>,
    theme: OscTheme,
    cmds: &mut Vec<OscCmd>,
) {
    use crate::danmaku_ui::{settings_card_bg, settings_card_outline, settings_card_rect};

    let screen = ui.max_rect();
    let layer = egui::LayerId::new(egui::Order::Foreground, Id::unique("osc-queue-card"));
    let mut lui = ui.new_child(
        egui::UiBuilder::new()
            .layer_id(layer)
            .max_rect(screen)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );

    let outside = lui.interact(screen, Id::unique("osc-queue-outside"), Sense::click());
    let close_now = outside.clicked()
        || outside.secondary_clicked()
        || lui.ctx().input(|i| i.key_pressed(egui::Key::Escape));

    const ROW_H: f32 = 40.0;
    let list_h = (st.queue_titles.len() as f32 * ROW_H).min(ROW_H * 9.0);
    let size = vec2(300.0, 18.0 + 26.0 + 6.0 + list_h + 18.0);
    let card = settings_card_rect(screen, anchor, size);
    let p = lui.painter();
    p.rect_filled(card, CornerRadius::same(14), settings_card_bg());
    p.rect_stroke(
        card.shrink(0.5),
        CornerRadius::same(14),
        Stroke::new(1.0, settings_card_outline()),
        StrokeKind::Inside,
    );
    let _ = lui.interact(card, Id::unique("osc-queue-card-hit"), Sense::click());

    let mut mui = lui.new_child(
        egui::UiBuilder::new()
            .id_salt(Id::unique("osc-queue-body"))
            .max_rect(card.shrink2(vec2(22.0, 18.0)))
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    mui.spacing_mut().item_spacing.y = 0.0;

    let mut close_x = false;
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 26.0), Sense::hover());
        let p = mui.painter();
        p.text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            "播放队列",
            osc_font(15.0),
            theme.text,
        );
        let x_rect = Rect::from_min_size(
            pos2(rect.right() - 24.0, rect.center().y - 12.0),
            vec2(24.0, 24.0),
        );
        let resp = mui.interact(x_rect, Id::unique("osc-queue-close"), Sense::click());
        let t = mui.ctx().animate_value_with_time(
            Id::unique("osc-queue-close-h"),
            resp.hovered() as u8 as f32,
            0.08,
        );
        if t > 0.01 {
            p.circle_filled(x_rect.center(), 11.0, fade(Color32::WHITE, 0.10 * t));
        }
        let c = mix(theme.text_dim, theme.text, t);
        let k = 3.4 + 0.4 * t;
        p.line_segment(
            [
                pos2(x_rect.center().x - k, x_rect.center().y - k),
                pos2(x_rect.center().x + k, x_rect.center().y + k),
            ],
            Stroke::new(1.4, c),
        );
        p.line_segment(
            [
                pos2(x_rect.center().x - k, x_rect.center().y + k),
                pos2(x_rect.center().x + k, x_rect.center().y - k),
            ],
            Stroke::new(1.4, c),
        );
        if resp.clicked() {
            close_x = true;
        }
    }

    mui.add_space(6.0);
    let mut jump_to: Option<usize> = None;
    egui::ScrollArea::vertical()
        .id_salt(Id::unique("osc-queue-list"))
        .max_height(list_h)
        .show_viewport(&mut mui, |ui, _| {
            for (i, title) in st.queue_titles.iter().enumerate() {
                if queue_row(ui, i, title, i + 1 == st.queue_pos, theme) {
                    jump_to = Some(i);
                }
            }
        });

    if close_x || close_now {
        cmds.push(OscCmd::TogglePanelQueue);
    }
    if let Some(i) = jump_to {
        cmds.push(OscCmd::PlayIndex(i));
    }
}

/// 队列行（browse_ui queue_row 同族：hover 微亮底 + 播放中 accent 图标 +
/// 截断文件名）。迁 osc.rs 以便 SW/GPU 共用（一套代码两处运行）。
pub(crate) fn queue_row(
    ui: &mut Ui,
    idx: usize,
    title: &str,
    current: bool,
    theme: OscTheme,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    let t = ui.ctx().animate_value_with_time(
        Id::unique("player-queue-row").with((idx, title)),
        resp.hovered() as u8 as f32,
        0.09,
    );
    let p = ui.painter();
    if t > 0.01 {
        p.rect_filled(rect, CornerRadius::same(8), fade(Color32::WHITE, 0.06 * t));
    }
    if current {
        p.rect_filled(rect, CornerRadius::same(8), fade(theme.accent, 0.10));
    }
    let icon_c = pos2(rect.left() + 21.0, rect.center().y);
    let icon_color = if current {
        theme.accent
    } else {
        fade(theme.accent, 0.9 * t)
    };
    if current || t > 0.01 {
        p.text(
            icon_c,
            Align2::CENTER_CENTER,
            icons::PLAY,
            osc_font(14.0),
            icon_color,
        );
    }
    let text_color = if current { theme.text } else { theme.text_dim };
    paint_left_center_truncated(
        p,
        pos2(rect.left() + 38.0, rect.center().y),
        title,
        osc_font(13.0),
        text_color,
        rect.width() - 38.0 - 14.0,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(title)
        .clicked()
}

/// 设置卡片（弹幕设置/队列/轨道菜单同族工艺：Foreground 层 + 深色半透明
/// 卡片 + osc_font + OscTheme）。锚定"设置"按钮上方；交互经命令桥回主线程
/// 执行（apply + save）。每帧从 OscState clone 配置快照，交互改本地副本，
/// changed() 发命令——主线程 exec 在 sync 之前，下一帧 osc_state 带新值，
/// 无双向同步问题。
fn settings_card(
    st: &OscState,
    ui: &mut Ui,
    anchor: Option<Pos2>,
    theme: OscTheme,
    cmds: &mut Vec<OscCmd>,
) {
    use crate::danmaku_ui::{settings_card_bg, settings_card_outline, settings_card_rect};
    use dyplayer_store::{StartupView, ThemeMode};

    let screen = ui.max_rect();
    let layer = egui::LayerId::new(egui::Order::Foreground, Id::unique("osc-settings-card"));
    let mut lui = ui.new_child(
        egui::UiBuilder::new()
            .layer_id(layer)
            .max_rect(screen)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );

    let outside = lui.interact(screen, Id::unique("osc-settings-outside"), Sense::click());
    let close_now = outside.clicked()
        || outside.secondary_clicked()
        || lui.ctx().input(|i| i.key_pressed(egui::Key::Escape));

    let size = vec2(320.0, (screen.height() - 40.0).clamp(480.0, 720.0));
    let card = settings_card_rect(screen, anchor, size);
    let p = lui.painter();
    p.rect_filled(card, CornerRadius::same(14), settings_card_bg());
    p.rect_stroke(
        card.shrink(0.5),
        CornerRadius::same(14),
        Stroke::new(1.0, settings_card_outline()),
        StrokeKind::Inside,
    );
    let _ = lui.interact(card, Id::unique("osc-settings-card-hit"), Sense::click());

    let mut mui = lui.new_child(
        egui::UiBuilder::new()
            .id_salt(Id::unique("osc-settings-body"))
            .max_rect(card.shrink2(vec2(22.0, 18.0)))
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    mui.spacing_mut().item_spacing.y = 0.0;

    let mut close_x = false;
    {
        let (rect, _) = mui.allocate_exact_size(vec2(mui.available_width(), 26.0), Sense::hover());
        let p = mui.painter();
        p.text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            "播放器设置",
            osc_font(15.0),
            theme.text,
        );
        let x_rect = Rect::from_min_size(
            pos2(rect.right() - 24.0, rect.center().y - 12.0),
            vec2(24.0, 24.0),
        );
        let resp = mui.interact(x_rect, Id::unique("osc-settings-close"), Sense::click());
        let t = mui.ctx().animate_value_with_time(
            Id::unique("osc-settings-close-h"),
            resp.hovered() as u8 as f32,
            0.08,
        );
        if t > 0.01 {
            p.circle_filled(x_rect.center(), 11.0, fade(Color32::WHITE, 0.10 * t));
        }
        let c = mix(theme.text_dim, theme.text, t);
        let k = 3.4 + 0.4 * t;
        p.line_segment(
            [
                pos2(x_rect.center().x - k, x_rect.center().y - k),
                pos2(x_rect.center().x + k, x_rect.center().y + k),
            ],
            Stroke::new(1.4, c),
        );
        p.line_segment(
            [
                pos2(x_rect.center().x - k, x_rect.center().y + k),
                pos2(x_rect.center().x + k, x_rect.center().y - k),
            ],
            Stroke::new(1.4, c),
        );
        if resp.clicked() {
            close_x = true;
        }
    }

    mui.add_space(6.0);

    let mut player = st.player_cfg.clone();
    let mut hwdec = st.engine_hwdec.clone();
    let mut ui_theme = st.ui_theme;
    let mut startup_view = st.startup_view;
    let mut danmaku_delay = st.danmaku_delay;
    let mut osc_theme_idx = st.theme_idx;
    let mut osc_layout = st.layout;
    let mut osc_nibble = st.nibble;
    let mut fx_glow = st.fx_glow;
    let mut fx_size = st.fx_size;
    let mut fx_color = st.fx_color;

    let mut player_changed = false;
    let mut hwdec_changed: Option<String> = None;
    let mut theme_changed: Option<ThemeMode> = None;
    let mut startup_changed: Option<StartupView> = None;
    let mut delay_changed: Option<f64> = None;
    let mut osc_prefs_changed = false;

    egui::ScrollArea::vertical()
        .id_salt(Id::unique("osc-settings-scroll"))
        .auto_shrink([false, false])
        .show(&mut mui, |ui| {
            ui.label(egui::RichText::new("播放").strong());
            player_changed |= ui.checkbox(&mut player.auto_next, "自动播放下一个").changed();
            ui.horizontal(|ui| {
                ui.label("快进");
                player_changed |= ui
                    .add(egui::DragValue::new(&mut player.seek_step_fwd).range(1..=600).suffix("s"))
                    .changed();
                ui.label("快退");
                player_changed |= ui
                    .add(egui::DragValue::new(&mut player.seek_step_back).range(1..=600).suffix("s"))
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("弹幕延迟");
                if ui
                    .add(egui::DragValue::new(&mut danmaku_delay).range(-600.0..=600.0).speed(0.5).suffix("s"))
                    .changed()
                {
                    delay_changed = Some(danmaku_delay);
                }
            });
            player_changed |= ui
                .checkbox(&mut player.show_stats_hud, "显示媒体信息块 (Tab)")
                .changed();
            player_changed |= ui
                .checkbox(
                    &mut player.display_pacing,
                    "平滑配速（按屏幕刷新率，实验性）",
                )
                .on_hover_text(
                    "开启 = display-resample：视频/弹幕按屏幕刷新率节拍，更平滑但高码率源更吃 CPU；\
                     关闭 = 音频时钟配速（video-sync=audio），省 CPU，卡顿时可作对照。",
                )
                .changed();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("解码方式");
                let current = HwdecPreset::from_mpv_value(&hwdec);
                for preset in HwdecPreset::ALL {
                    if ui
                        .selectable_label(current == preset, preset.label())
                        .on_hover_text(match preset {
                            HwdecPreset::Auto => "自动（mpv hwdec=auto-safe）：优先硬件解码，仅用已知安全方式",
                            HwdecPreset::Hardware => "强制硬件解码（mpv hwdec=d3d11va）：编码不受硬件支持时会回退软解",
                            HwdecPreset::Software => "软件解码（mpv hwdec=no）：纯 CPU 解码",
                        })
                        .clicked()
                    {
                        hwdec = preset.mpv_value().into();
                        hwdec_changed = Some(hwdec.clone());
                    }
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("渲染路径");
                ui.label(
                    egui::RichText::new(if st.engine_render_path.eq_ignore_ascii_case("gpu") {
                        "GPU（在主页设置中切换）"
                    } else {
                        "SW（在主页设置中切换）"
                    })
                    .color(egui::Color32::from_gray(140))
                    .size(12.0),
                )
                .on_hover_text("渲染路径是全局引擎配置（会话重建型切换），已迁至媒体库页的设置视图");
            });
            let mut startup_player = startup_view == StartupView::Player;
            if ui
                .checkbox(&mut startup_player, "启动时进入播放页")
                .on_hover_text("应用启动后直接显示播放页，可拖入视频文件或文件夹播放")
                .changed()
            {
                startup_view = if startup_player {
                    StartupView::Player
                } else {
                    StartupView::Library
                };
                startup_changed = Some(startup_view);
            }

            ui.separator();
            ui.label(egui::RichText::new("语言偏好（对之后加载的视频生效）").strong());
            ui.horizontal(|ui| {
                ui.label("音频");
                player_changed |= ui
                    .text_edit_singleline(&mut player.pref_audio_lang)
                    .on_hover_text("mpv alang，如 zh,eng；留空使用 mpv 默认")
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("字幕");
                player_changed |= ui
                    .text_edit_singleline(&mut player.pref_sub_lang)
                    .on_hover_text("mpv slang，如 chi,zh-Hans；留空使用 mpv 默认")
                    .changed();
            });
            player_changed |= ui
                .checkbox(&mut player.sub_disabled_by_default, "默认不加载字幕")
                .changed();

            ui.separator();
            ui.add_space(4.0);
            ui.label(egui::RichText::new("外观").strong());
            ui.horizontal(|ui| {
                for (mode, label) in [
                    (ThemeMode::System, "跟随系统"),
                    (ThemeMode::Light, "浅色"),
                    (ThemeMode::Dark, "深色"),
                ] {
                    if ui
                        .selectable_label(ui_theme == mode, label)
                        .on_hover_text(format!(
                            "{label}{}",
                            if mode == ThemeMode::System {
                                "（Windows 深色模式开关）"
                            } else {
                                ""
                            }
                        ))
                        .clicked()
                    {
                        ui_theme = mode;
                        theme_changed = Some(mode);
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(egui::RichText::new("OSC 主题").strong());
            ui.horizontal(|ui| {
                for (i, th) in THEMES.iter().enumerate() {
                    let selected = osc_theme_idx == i;
                    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
                    let p = ui.painter();
                    p.rect_filled(rect.shrink(3.0), CornerRadius::same(7), th.accent);
                    if selected {
                        p.rect_stroke(
                            rect.shrink(1.0),
                            CornerRadius::same(8),
                            Stroke::new(2.0, Color32::WHITE),
                            StrokeKind::Inside,
                        );
                    }
                    if resp.clicked() {
                        osc_theme_idx = i;
                        osc_prefs_changed = true;
                    }
                    resp.on_hover_text(th.name);
                }
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new("布局").strong());
            ui.horizontal(|ui| {
                if ui.selectable_label(osc_layout == OscLayout::Default, "default").clicked() {
                    osc_layout = OscLayout::Default;
                    osc_prefs_changed = true;
                }
                if ui.selectable_label(osc_layout == OscLayout::Mini, "mini").clicked() {
                    osc_layout = OscLayout::Mini;
                    osc_prefs_changed = true;
                }
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new("章节标记").strong());
            ui.horizontal(|ui| {
                for (style, name) in [
                    (OscNibble::Gap, "gap"),
                    (OscNibble::Bar, "bar"),
                    (OscNibble::Triangle, "triangle"),
                ] {
                    if ui.selectable_label(osc_nibble == style, name).clicked() {
                        osc_nibble = style;
                        osc_prefs_changed = true;
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new("悬停效果 (hover_effect)").strong());
            if ui.checkbox(&mut fx_glow, "glow 光晕").changed() {
                osc_prefs_changed = true;
            }
            if ui.checkbox(&mut fx_size, "size 放大").changed() {
                osc_prefs_changed = true;
            }
            if ui.checkbox(&mut fx_color, "color 变色").changed() {
                osc_prefs_changed = true;
            }

            ui.separator();
            ui.label("快捷键：空格 播放/暂停 · ←/→ 快退/快进 · ↑/↓ 音量 · Tab 信息块 · N/P 下/上一集 · D 弹幕 · S 截图");
        });

    if close_x || close_now {
        cmds.push(OscCmd::TogglePanelSettings);
    }
    if player_changed {
        cmds.push(OscCmd::SetPlayerConfig(player));
    }
    if let Some(h) = hwdec_changed {
        cmds.push(OscCmd::SetHwdec(h));
    }
    if let Some(t) = theme_changed {
        cmds.push(OscCmd::SetTheme(t));
    }
    if let Some(s) = startup_changed {
        cmds.push(OscCmd::SetStartupView(s));
    }
    if let Some(d) = delay_changed {
        cmds.push(OscCmd::SetDanmakuDelay(d));
    }
    if osc_prefs_changed {
        cmds.push(OscCmd::SetOscPrefs {
            theme_idx: osc_theme_idx,
            layout: osc_layout,
            nibble: osc_nibble,
            fx_glow,
            fx_size,
            fx_color,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_icon_codepoint_matches_spec() {
        assert_eq!(icons::SETTINGS, '\u{F072}');
    }

    #[test]
    fn speed_label_trims_integral() {
        assert_eq!(speed_label(1.0), "1x");
        assert_eq!(speed_label(1.5), "1.5x");
        assert_eq!(speed_label(0.75), "0.75x");
    }
}
