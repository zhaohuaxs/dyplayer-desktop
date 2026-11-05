//! glow 后端空实现（老设备版 SW-only）：无覆盖层。
//! OverlayHandle::spawn() 恒返 None → overlay 恒 None →
//! 弹幕/OSC/HUD 画主窗口 egui（SW 路径现状行为）。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dyplayer_store::DanmakuConfig;
use dyplayer_mpv::ClockSnapshot;

pub enum OverlayLoad {
    Auto {
        video: PathBuf,
        preferred: Option<PathBuf>,
    },
    XmlPath(PathBuf),
    Content(String),
    Clear,
}

#[derive(Clone, Copy)]
pub struct OverlayRunState {
    pub enabled: bool,
    pub show_top: bool,
    pub show_bottom: bool,
    pub opacity: f32,
    pub delay_seconds: f64,
    pub gradient: bool,
    pub gradient_inset: f64,
}

impl Default for OverlayRunState {
    fn default() -> Self {
        Self {
            enabled: false,
            show_top: false,
            show_bottom: false,
            opacity: 1.0,
            delay_seconds: 0.0,
            gradient: false,
            gradient_inset: 0.0,
        }
    }
}

pub struct OverlayBridge {
    pub media_time: f64,
    pub clock_snapshot: ClockSnapshot,
    pub paused: bool,
    pub visible: bool,
    pub danmaku_settings_toggle: bool,
    pub danmaku_settings_close: bool,
    pub danmaku_settings_open: bool,
    pub danmaku_file_result: Option<Option<PathBuf>>,
    pub font_file_result: Option<Option<PathBuf>>,
    pub ppp: f32,
    pub screen_rect_px: [i32; 4],
    pub cfg_version: u64,
    pub danmaku_cfg: DanmakuConfig,
    pub run_state: OverlayRunState,
    pub run_state_hold: bool,
    pub danmaku_delay: Option<f64>,
    pub load_request: Option<OverlayLoad>,
    pub osc_state: Option<crate::osc_state::OscState>,
    pub osc_cmds: Vec<crate::osc_state::OscCmd>,
    pub osc_btn_rect: Option<egui::Rect>,
    pub close_track_menu: bool,
    pub osc_wake: bool,
    pub mouse_events: Vec<egui::Event>,
    pub key_events: Vec<egui::Event>,
    pub host_offset: egui::Vec2,
    pub owner_hwnd: isize,
    pub item_count: usize,
    pub visible_count: usize,
    pub fps: f64,
    pub dm_fps: f64,
    pub frame_counter: u64,
    pub config_flush_request: bool,
    pub config_flush_result: Option<DanmakuConfig>,
    pub overlay_has_focus: bool,
}

impl OverlayBridge {
    pub fn new(cfg: DanmakuConfig) -> Self {
        Self {
            media_time: 0.0,
            clock_snapshot: ClockSnapshot::default(),
            paused: false,
            visible: true,
            danmaku_settings_toggle: false,
            danmaku_settings_close: false,
            danmaku_settings_open: false,
            danmaku_file_result: None,
            font_file_result: None,
            ppp: 1.0,
            screen_rect_px: [0, 0, 0, 0],
            cfg_version: 1,
            run_state: OverlayRunState::default(),
            danmaku_cfg: cfg,
            run_state_hold: false,
            danmaku_delay: None,
            load_request: None,
            osc_state: None,
            osc_cmds: Vec::new(),
            osc_btn_rect: None,
            close_track_menu: false,
            osc_wake: false,
            mouse_events: Vec::new(),
            key_events: Vec::new(),
            host_offset: egui::Vec2::ZERO,
            owner_hwnd: 0,
            item_count: 0,
            visible_count: 0,
            fps: 0.0,
            dm_fps: 0.0,
            frame_counter: 0,
            config_flush_request: false,
            config_flush_result: None,
            overlay_has_focus: false,
        }
    }
}

pub struct OverlayHandle {
    bridge: Arc<Mutex<OverlayBridge>>,
}

impl OverlayHandle {
    pub fn spawn(_cfg: DanmakuConfig) -> Option<Self> {
        None
    }
    pub fn bridge(&self) -> &Arc<Mutex<OverlayBridge>> {
        &self.bridge
    }
    pub fn push_config(&self, _cfg: &DanmakuConfig) {}
}

pub(crate) fn push_host_mouse_if_overlay(_msg: u32, _wp: usize, _lp: isize) -> bool {
    false
}