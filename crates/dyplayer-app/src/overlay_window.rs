//! M2 弹幕覆盖窗口（GPU wid 渲染路径专用，docs/m2-overlay-danmaku.md §2）。
//!
//! 独立线程承载：raw Win32 透明窗（`WS_EX_NOREDIRECTIONBITMAP|LAYERED|
//! TRANSPARENT|NOACTIVATE|TOOLWINDOW`——全穿透、不抢焦点、不进任务栏）
//! + wgpu `PreMultiplied` surface（DComp visual swapchain，
//!   `Dx12SwapchainKind::DxgiFromVisual`——透明窗口必需，DxgiFromHwnd 在
//!   NOREDIRECTIONBITMAP 窗口上 configure 死挂，PoC-2 实证）+ 独立 egui
//!   Context + `egui_wgpu::Renderer` + 本线程 `DanmakuOverlay` 实例。
//!
//! **不用 winit**：winit Windows 每进程只允许一个 EventLoop（eframe 主
//! 线程已占用，二次创建报 "EventLoop can't be recreated"——M2a 实测）；
//! 覆盖窗口是纯 Win32 需求（零输入/无边框/位置跟随），自建消息循环
//! （PeekMessage + 16ms 节拍）即可，surface 经手工 RawHandle 创建。
//!
//! 弹幕渲染是自带字形 atlas 的 `Shape::Mesh`（dyplayer-danmaku 光栅化），
//! 不依赖 egui 字体链——覆盖 ctx 无需装 CJK 字体。
//!
//! 线程模型：主线程每帧写 [`OverlayBridge`]（media_time / 暂停态 / 屏幕
//! 矩形 / 配置版本 / 弹幕加载请求），覆盖线程只读；弹幕统计（条数/可见数）
//! 由覆盖线程回写供诊断。无交互回传（覆盖层零交互，穿透到 mpv 宿主窗口）。
//!
//! PoC-2 坑清单（全部已规避）：透明窗口必须 DxgiFromVisual；wgpu 30 用
//! `queue.present(frame)`；`CurrentSurfaceTexture` 是 enum。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dyplayer_store::DanmakuConfig;
use dyplayer_mpv::ClockSnapshot;

use crate::danmaku_ui::DanmakuOverlay;

/// 覆盖窗口类名（每进程注册一次）。
const CLASS_NAME: &str = "DyPlayerDanmakuOverlay";

const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;

const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_NOREDIRECTIONBITMAP: u32 = 0x0200_0000;

const PM_REMOVE: u32 = 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Msg {
    hwnd: usize,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

unsafe extern "system" {
    fn GetModuleHandleW(lp: *const u16) -> usize;
    fn RegisterClassW(wc: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32,
        cls: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: usize,
        menu: usize,
        inst: usize,
        param: *mut core::ffi::c_void,
    ) -> usize;
    fn DefWindowProcW(hwnd: usize, msg: u32, wp: usize, lp: isize) -> isize;
    fn DestroyWindow(hwnd: usize) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: usize, min: u32, max: u32, remove: u32) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn SetCapture(hwnd: usize) -> usize;
    fn ReleaseCapture() -> i32;
}

#[repr(C)]
struct WndClassW {
    style: u32,
    proc: usize,
    cls_extra: i32,
    wnd_extra: i32,
    inst: usize,
    icon: usize,
    cursor: usize,
    background: usize,
    menu_name: *const u16,
    class_name: *const u16,
}

/// 覆盖层自身鼠标队列（去 WS_EX_LAYERED 后 hit test 按窗口矩形——
/// 覆盖层是真实命中窗口）。(msg, x_px, y_px) 覆盖层客户区像素。
/// LazyLock：任一线程首次访问时初始化——OnceLock + get() 不 set 的话
/// 永远返回 None，push 静默丢弃（批 2 实测踩坑）。
static OVERLAY_MOUSE: std::sync::LazyLock<std::sync::Mutex<Vec<(u32, i32, i32)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// 宿主窗口（wid_host）鼠标队列：视频区被覆盖层盖住时鼠标到不了宿主，
/// 此队列兜底（万一覆盖层缺帧/隐藏期残留）；正常主通道是 OVERLAY_MOUSE。
static MOUSE_EVENTS: std::sync::LazyLock<std::sync::Mutex<Vec<(u32, i32, i32)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MOUSEWHEEL: u32 = 0x020A;

/// R19：滚轮事件队列（wheel delta；正=向上，负=向下）。
static OVERLAY_WHEEL: std::sync::LazyLock<std::sync::Mutex<Vec<i32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));
static HOST_WHEEL: std::sync::LazyLock<std::sync::Mutex<Vec<i32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// F11：队列满时采样告警（每 100 次 warn 一次，避免 wnd_proc 热路径刷屏）。
fn queue_full_warn(kind: &'static str) {
    static HOST_MOUSE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static HOST_WHEEL_C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static OV_MOUSE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static OV_WHEEL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = match kind {
        "host_mouse" => &HOST_MOUSE,
        "host_wheel" => &HOST_WHEEL_C,
        "ov_mouse" => &OV_MOUSE,
        _ => &OV_WHEEL,
    };
    use std::sync::atomic::Ordering;
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 100 == 1 {
        tracing::warn!(target: "overlay", "{kind} 队列满，累计丢弃 {n} 次（消费端可能卡顿）");
    }
}

/// F10：OverlayBridge poison 恢复——取中间状态继续运行，只 error 一次避免刷屏。
fn lock_bridge(
    bridge: &std::sync::Mutex<OverlayBridge>,
) -> std::sync::MutexGuard<'_, OverlayBridge> {
    bridge.lock().unwrap_or_else(|e| {
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::error!(target: "overlay", "overlay bridge poisoned, recovering");
        }
        e.into_inner()
    })
}


/// 宿主窗口（wid_host）wnd_proc 调用：视频区鼠标消息入队（供覆盖线程
/// 转译 egui input）。返回 true = 已消费（宿主不再 DefWindowProc）。
/// 坐标原样（宿主客户区像素）——换算在 take_mouse_events（桥 offset）。
/// 弹幕设置卡片（迁覆盖层后）打开期间照常转发：卡片自身在覆盖层 egui
/// 中，外部命中层收得到点击（无黑洞）。
pub(crate) fn push_host_mouse_if_overlay(msg: u32, wp: usize, lp: isize) -> bool {
    if matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
    ) {
        let x = ((lp & 0xFFFF) as u16) as i16 as i32;
        let y = (((lp >> 16) & 0xFFFF) as u16) as i16 as i32;
        if let Ok(mut q) = MOUSE_EVENTS.lock()
            && q.len() < 256
        {
            q.push((msg, x, y));
        } else {
            queue_full_warn("host_mouse");
        }
        return true;
    }
    // R19：滚轮事件从宿主窗口转发。
    if msg == WM_MOUSEWHEEL {
        let delta = ((wp >> 16) & 0xFFFF) as u16 as i16 as i32;
        if let Ok(mut q) = HOST_WHEEL.lock()
            && q.len() < 64
        {
            q.push(delta);
        } else {
            queue_full_warn("host_wheel");
        }
        return true;
    }
    false
}

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: usize,
    msg: u32,
    wp: usize,
    lp: isize,
) -> isize {
    // 鼠标事件入队（render 每帧转译 egui input）。按住拖出窗口期间
    // SetCapture 保证事件继续到达（egui press/release 配对语义）。
    if matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
    ) {
        let x = ((lp & 0xFFFF) as u16) as i16 as i32;
        let y = (((lp >> 16) & 0xFFFF) as u16) as i16 as i32;
        if let Ok(mut q) = OVERLAY_MOUSE.lock()
            && q.len() < 256
        {
            q.push((msg, x, y));
        } else {
            queue_full_warn("ov_mouse");
        }
        match msg {
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => unsafe {
                SetCapture(hwnd);
            },
            WM_LBUTTONUP | WM_RBUTTONUP => unsafe {
                ReleaseCapture();
            },
            _ => {}
        }
        return 0;
    }
    // R19：滚轮事件入队（delta；覆盖线程转译为 egui::Event::Scroll）。
    if msg == WM_MOUSEWHEEL {
        let delta = ((wp >> 16) & 0xFFFF) as u16 as i16 as i32;
        if let Ok(mut q) = OVERLAY_WHEEL.lock()
            && q.len() < 64
        {
            q.push(delta);
        } else {
            queue_full_warn("ov_wheel");
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 取出鼠标事件并转译为 egui input events。双源合并：
/// - 覆盖层自身队列（主通道；坐标直 /ppp）；
/// - 宿主队列（兜底；坐标 + host_offset 换算——宿主客户原点在覆盖层
///   逻辑系中的偏移由主线程每帧写桥）。
/// - WM_LBUTTONDBLCLK 转译为一次 press+release 对（CS_DBLCLKS 下
///   Windows 以 DBLCLK 替代第二次 down——egui 靠两次快 press 判定
///   double_clicked）。
fn take_mouse_events(ppp: f32, host_offset: egui::Vec2) -> Vec<egui::Event> {
    type MouseQueue = std::sync::LazyLock<std::sync::Mutex<Vec<(u32, i32, i32)>>>;
    let take_queue = |lock: &MouseQueue| {
        lock.lock()
            .ok()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    };
    let own = take_queue(&OVERLAY_MOUSE);
    let host = take_queue(&MOUSE_EVENTS);
    // 双源统一为 (msg, x, y, offset)：own 直 /ppp；host + host_offset。
    let merged: Vec<(u32, i32, i32, egui::Vec2)> = own
        .into_iter()
        .map(|(m, x, y)| (m, x, y, egui::Vec2::ZERO))
        .chain(host.into_iter().map(|(m, x, y)| (m, x, y, host_offset)))
        .collect();
    let to_pos = |x: i32, y: i32, off: egui::Vec2| {
        egui::pos2(x as f32 / ppp + off.x, y as f32 / ppp + off.y)
    };
    let mut out = Vec::with_capacity(merged.len());
    for (msg, x, y, off) in merged {
        let pos = to_pos(x, y, off);
        use egui::Event as E;
        use egui::PointerButton as PB;
        match msg {
            WM_MOUSEMOVE => out.push(E::PointerMoved(pos)),
            WM_LBUTTONDOWN => out.push(E::PointerButton {
                pos,
                button: PB::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }),
            WM_LBUTTONUP => out.push(E::PointerButton {
                pos,
                button: PB::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }),
            WM_LBUTTONDBLCLK => {
                out.push(E::PointerButton {
                    pos,
                    button: PB::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                });
                out.push(E::PointerButton {
                    pos,
                    button: PB::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                });
            }
            WM_RBUTTONDOWN => out.push(E::PointerButton {
                pos,
                button: PB::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }),
            WM_RBUTTONUP => out.push(E::PointerButton {
                pos,
                button: PB::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }),
            _ => {}
        }
    }
    // R19：滚轮事件转译（双源合并；delta/120 * 24 ≈ 一格 24 逻辑像素）。
    let take_wheel = |lock: &std::sync::LazyLock<std::sync::Mutex<Vec<i32>>>| {
        lock.lock()
            .ok()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    };
    for delta in take_wheel(&OVERLAY_WHEEL).into_iter().chain(take_wheel(&HOST_WHEEL)) {
        out.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, delta as f32 / 120.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: egui::Modifiers::default(),
        });
    }
    out
}

/// 主线程 → 覆盖线程的弹幕加载请求。
pub enum OverlayLoad {
    /// 同名自动加载（FileLoaded；preferred = DB 绑定优先路径）。
    Auto {
        video: PathBuf,
        preferred: Option<PathBuf>,
    },
    /// 手动指定弹幕文件（拖入 XML / 弹窗选择 / 绑定恢复）。
    XmlPath(PathBuf),
    /// 远程预取的 XML 内容（后台线程下载完成）。
    Content(String),
    /// 换集窗口期清空。
    Clear,
}

/// 弹幕运行态（即时项，主线程每帧写、覆盖线程每帧生效，不触发重排）。
#[derive(Clone, Copy)]
pub struct OverlayRunState {
    pub enabled: bool,
    /// 顶部弹幕（FixTop）开关（查询侧过滤即时生效）。
    pub show_top: bool,
    /// 底部弹幕（FixBottom）开关（查询侧过滤即时生效）。
    pub show_bottom: bool,
    pub opacity: f32,
    pub delay_seconds: f64,
    pub gradient: bool,
    pub gradient_inset: f64,
}

/// 跨线程状态桥（主线程写几何/时间/配置/加载请求；覆盖线程回写统计）。
/// 临界区极短（字段读写），60Hz 双侧加锁无竞争问题。
pub struct OverlayBridge {
    /// MediaClock 媒体时间（主线程每帧写；暂停时冻结）。
    pub media_time: f64,
    /// MediaClock 原子快照（R08：覆盖线程自行外推，不依赖主线程节拍）。
    pub clock_snapshot: ClockSnapshot,
    pub paused: bool,
    /// 覆盖窗口可见性（false = 隐藏窗口）。
    pub visible: bool,
    /// 主线程 → 覆盖线程：切换弹幕设置卡片（OSC 右键命令；卡片迁覆盖层
    /// 后主线程不再持有开态）。
    pub danmaku_settings_toggle: bool,
    /// 主线程 → 覆盖线程：关闭弹幕设置卡片（ESC/面板互斥/换集/全屏）。
    pub danmaku_settings_close: bool,
    /// 覆盖线程 → 主线程：弹幕设置卡片开态回传（主线程据此跳过 run_state
    /// 写入——设置滑条拖动中覆盖层本地即时项不能被主线程旧值覆盖）。
    pub danmaku_settings_open: bool,
    /// 主线程 → 覆盖线程：rfd 弹幕文件选择结果（PickDanmakuFile 命令的
    /// 回程；外层 Some=有结果，内层 Some=选定路径 / None=取消）。
    pub danmaku_file_result: Option<Option<PathBuf>>,
    /// 主线程 → 覆盖线程：rfd 字体文件选择结果（PickDanmakuFont 回程，
    /// 同构）。
    pub font_file_result: Option<Option<PathBuf>>,
    /// 主窗口 pixels_per_point（egui 逻辑坐标换算）。
    pub ppp: f32,
    /// 覆盖窗口屏幕物理矩形（l/t/r/b；主窗口客户区原点 + 弹幕区域 × ppp）。
    pub screen_rect_px: [i32; 4],
    /// danmaku_cfg 内容版本（布局项：字号/区域/速度/描边/字体——变化时
    /// 覆盖线程重新 apply_config 触发异步重排）。
    pub cfg_version: u64,
    pub danmaku_cfg: DanmakuConfig,
    /// 即时项运行态（每帧同步：开关/透明度/延迟/渐变，即时生效）。
    pub run_state: OverlayRunState,
    /// 覆盖线程 → 主线程：卡片关闭沿 hold 一帧。关闭沿覆盖线程把卡片内
    /// 最终即时项写回 run_state；主线程 SaveDanmakuConfig 消费（exec 在
    /// sync 之后）前 sync 据此跳过写入，防覆盖线程下帧读到主线程旧值
    /// 闪回一帧。主线程读到即清，下一帧恢复正常写入（回环已 apply 同值）。
    pub run_state_hold: bool,
    /// 覆盖线程 → 主线程：卡片关闭沿回传的弹幕延迟（会话级即时项，不在
    /// DanmakuConfig——主线程实例消费防关闭后被旧值打回）。
    pub danmaku_delay: Option<f64>,
    /// 弹幕加载请求（消费后覆盖线程置 None）。
    pub load_request: Option<OverlayLoad>,
    /// OSC 渲染快照（主线程每帧写 Some；None = 尚未就绪，覆盖线程跳过 OSC）。
    pub osc_state: Option<crate::osc_state::OscState>,
    /// 覆盖线程 OSC 交互产生的命令（覆盖线程 push，主线程 ui() 消费）。
    pub osc_cmds: Vec<crate::osc_state::OscCmd>,
    /// 覆盖线程回传的"弹幕"按钮矩形（主线程弹幕设置弹窗锚点）。
    pub osc_btn_rect: Option<egui::Rect>,
    /// 主线程 → 覆盖线程：关闭轨道菜单（面板互斥/换集/全屏切换时；
    /// 菜单状态在覆盖线程 OscUiState，主线程碰不到）。
    pub close_track_menu: bool,
    /// 主线程 → 覆盖线程：唤醒 OSC（键盘快捷键触发；GPU 下显隐状态机
    /// 在覆盖线程，主线程 osc_wake 无效）。
    pub osc_wake: bool,
    /// 主线程 → 覆盖线程：鼠标事件镜像。DComp 透明窗 alpha=0 区域
    /// hit test 天然穿透（Windows 语义）——播放区鼠标实际落在宿主窗口，
    /// 宿主 wnd_proc 入队（push_host_mouse_if_overlay）+ 主窗口 egui
    /// 事件镜像（黑边区直达主窗口）双源汇入，覆盖线程拼进 RawInput。
    pub mouse_events: Vec<egui::Event>,
    /// 主线程 → 覆盖线程：键盘事件转发。GPU 路径下键盘只到主窗口 egui
    /// ctx，覆盖层 ctx 收不到——当覆盖层有焦点控件（TextEdit）时，主线程
    /// handle_shortcuts 把 Event::Key/Event::Text 取出经桥转发，覆盖线程
    /// 拼进 RawInput（与 mouse_events 同构：主线程写、覆盖线程消费清空）。
    pub key_events: Vec<egui::Event>,
    /// 宿主客户区原点在覆盖层逻辑坐标系中的偏移（= video_rect.min −
    /// avail_full.min，主线程每帧写；宿主鼠标换算用）。
    pub host_offset: egui::Vec2,
    /// 覆盖层 owner（主窗口 HWND；0 = 未设置）。
    ///
    /// owned window 语义（Win32 官方）：owned 永远浮在 owner 之上、随
    /// owner 最小化、不盖其他应用——比 topmost 优。覆盖线程读到非 0
    /// 且与当前 owner 不同时经 `GWLP_HWNDPARENT` 挂 owner（改 owner 不
    /// 改 parent/坐标系，与 SetParent 无关），并 `SetWindowPos(HWND_TOP)`
    /// 重申一次 z 序（修"主窗口激活后覆盖层被压底"——NOACTIVATE 窗口
    /// 永不被激活，每次激活主窗口都会把它压到非 topmost 组底）。
    pub owner_hwnd: isize,
    // ---- 覆盖线程回写（主线程诊断/心跳用）----
    pub item_count: usize,
    pub visible_count: usize,
    pub fps: f64,
    /// 弹幕帧率（覆盖层统计：有可见弹幕提交的帧率，0.5s 窗口——与主线程
    /// SW 统计同语义；GPU 下主线程 DanmakuOverlay 不渲染，其 dm_fps 恒 0，
    /// HUD"弹幕"行经此字段取值）。
    pub dm_fps: f64,
    /// 覆盖线程心跳（帧计数；主线程判活）。
    pub frame_counter: u64,
    /// R20B：主线程退出时请求覆盖线程导出最终配置（卡片未关闭场景）。
    pub config_flush_request: bool,
    /// R20B：覆盖线程导出的最终配置（主线程 on_exit 读取后保存）。
    pub config_flush_result: Option<DanmakuConfig>,
    /// R19：覆盖层 egui ctx 有焦点控件（TextEdit 等）——主线程据此跳过
    /// 播放器快捷键，避免覆盖层输入时空格暂停等误触发。
    pub overlay_has_focus: bool,
}

impl OverlayBridge {
    fn new(cfg: DanmakuConfig) -> Self {
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
            run_state: OverlayRunState {
                enabled: cfg.enabled,
                show_top: cfg.show_top,
                show_bottom: cfg.show_bottom,
                opacity: cfg.opacity,
                delay_seconds: 0.0,
                gradient: cfg.gradient,
                gradient_inset: f64::from(cfg.gradient_inset),
            },
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

/// 覆盖线程句柄（PlayerSession 持有；Drop 时通知退出并 join）。
pub struct OverlayHandle {
    bridge: Arc<Mutex<OverlayBridge>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OverlayHandle {
    /// 启动覆盖线程（gpu wid 路径；失败回退主线程弹幕不渲染——M1 限制降级）。
    pub fn spawn(cfg: DanmakuConfig) -> Option<Self> {
        let bridge = Arc::new(Mutex::new(OverlayBridge::new(cfg)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_bridge = Arc::clone(&bridge);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("dyplayer-danmaku-overlay".into())
            .spawn(move || {
                overlay_main(thread_bridge, thread_stop);
            })
            .ok();
        let thread = match thread {
            Some(t) => t,
            None => {
                tracing::warn!("弹幕覆盖线程 spawn 失败（GPU 路径弹幕不可用）");
                return None;
            }
        };
        Some(Self {
            bridge,
            stop,
            thread: Some(thread),
        })
    }

    pub fn bridge(&self) -> &Arc<Mutex<OverlayBridge>> {
        &self.bridge
    }

    /// 主线程侧推送配置变更（cfg_version 递增；覆盖线程 apply_config）。
    pub fn push_config(&self, cfg: &DanmakuConfig) {
        if let Ok(mut b) = self.bridge.lock() {
            b.cfg_version = b.cfg_version.wrapping_add(1);
            b.danmaku_cfg = cfg.clone();
        }
    }
}

impl Drop for OverlayHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // 覆盖线程 60fps 自续，单帧内必查 stop；join 上限防卡 UI 线程。
            let _ = t.join();
        }
    }
}

/// 覆盖线程主体：raw Win32 窗口 + wgpu + egui_wgpu + DanmakuOverlay。
/// winit 不可用（进程唯一 EventLoop 已被 eframe 占用），消息循环自管。
fn overlay_main(bridge: Arc<Mutex<OverlayBridge>>, stop: Arc<AtomicBool>) {
    let (cfg0, rect0, visible0) = {
        let b = lock_bridge(&bridge);
        (b.danmaku_cfg.clone(), b.screen_rect_px, b.visible)
    };
    let hwnd = match create_overlay_window(rect0, visible0) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("弹幕覆盖窗口创建失败: {e}");
            return;
        }
    };
    let (w, h) = ((rect0[2] - rect0[0]).max(1), (rect0[3] - rect0[1]).max(1));
    let gpu = match init_overlay_gpu(hwnd, w as u32, h as u32) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("弹幕覆盖层 GPU 初始化失败: {e}");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }
    };
    let mut danmaku = DanmakuOverlay::new();
    danmaku.apply_config(&cfg0);
    // 覆盖层实例启用 defer 文件对话框模式：设置卡片的 rfd 请求转
    // OscCmd 命令由主线程弹（覆盖线程无窗口模态能力）。
    danmaku.defer_file_dialogs = true;

    tracing::info!("M2 弹幕覆盖层已启动（透明 DComp 窗口 + PreMultiplied）");

    let mut state = OverlayState {
        bridge,
        stop,
        hwnd,
        gpu,
        danmaku,
        osc_ui: crate::osc_state::OscUiState::default(),

        applied_cfg_version: 0,
        applied_owner: 0,
        last_rect_px: rect0,
        last_visible: Some(visible0),
        start: Instant::now(),
        frame_count: 0,
        last_fps_sample: Instant::now(),
        dm_frame_counter: 0,
        dm_fps: 0.0,
        fps: 0.0,
    };
    state.run_loop();
}

struct OverlayState {
    bridge: Arc<Mutex<OverlayBridge>>,
    stop: Arc<AtomicBool>,
    hwnd: usize,
    gpu: OverlayGpu,
    danmaku: DanmakuOverlay,
    /// OSC 交互瞬态（覆盖线程实例：显隐状态机/seek 预览/轨道菜单/锚点）。
    osc_ui: crate::osc_state::OscUiState,


    applied_cfg_version: u64,
    /// 已挂接的 owner HWND（0 = 尚未；≠ 桥值时重新挂接）。
    applied_owner: isize,
    last_rect_px: [i32; 4],
    last_visible: Option<bool>,
    start: Instant,
    frame_count: u64,
    last_fps_sample: Instant,
    fps: f64,
    /// 弹幕帧率统计（0.5s 窗口：有可见弹幕的帧数；与主线程 SW 统计同语义）。
    dm_frame_counter: u64,
    dm_fps: f64,
}

struct OverlayGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    ctx: egui::Context,
}

impl OverlayState {
    fn run_loop(&mut self) {
        let mut msg = Msg {
            hwnd: 0,
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            pt_x: 0,
            pt_y: 0,
        };
        while !self.stop.load(Ordering::Relaxed) {
            unsafe {
                while PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE) != 0 {

                    let _ = DispatchMessageW(&msg);
                }
            }
            self.render();
            // 60fps 节拍（wgpu Fifo present 本身 vsync 阻塞对齐，sleep 兜底）
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    /// 每帧：读桥 → 同步窗口几何/配置/加载 → egui pass 弹幕 → wgpu 渲染。
    fn render(&mut self) {
        // ---- 读桥 ----
        let (clock_snapshot, visible, ppp, rect, cfg_version, run_state, load_request, owner, settings_toggle, settings_close) = {
            let mut b = lock_bridge(&self.bridge);
            b.frame_counter = self.frame_count;
            b.item_count = self.danmaku.item_count();
            b.visible_count = self.danmaku.last_visible;
            b.fps = self.fps;
            b.dm_fps = self.dm_fps;
            let toggle = b.danmaku_settings_toggle;
            let close = b.danmaku_settings_close;
            b.danmaku_settings_toggle = false;
            b.danmaku_settings_close = false;
            (
                b.clock_snapshot.clone(),
                b.visible,
                b.ppp,
                b.screen_rect_px,
                b.cfg_version,
                b.run_state,
                b.load_request.take(),
                b.owner_hwnd,
                toggle,
                close,
            )
        };
        // R08：覆盖线程自行外推 media_time，不依赖主线程每帧推送的 f64。
        let media_time = clock_snapshot.media_time_at(Instant::now());

        // ---- 弹幕设置卡片开关（OSC 右键命令 / 主线程 ESC/互斥关闭）----
        let settings_was_open = self.danmaku.settings_open;
        if settings_toggle {
            self.danmaku.settings_open = !self.danmaku.settings_open;
        }
        if settings_close && self.danmaku.settings_open {
            self.danmaku.settings_open = false;
        }

        // ---- owner 挂接（主窗口 HWND 首帧到位后一次性）----
        if owner != 0 && owner != self.applied_owner {
            set_window_owner(self.hwnd, owner);
            bring_to_top(self.hwnd);
            self.applied_owner = owner;
        }

        // ---- 窗口几何/可见性跟随 ----
        if rect != self.last_rect_px {
            let (w, h) = ((rect[2] - rect[0]).max(1), (rect[3] - rect[1]).max(1));
            move_window(self.hwnd, rect[0], rect[1], w, h);
            self.gpu.config.width = w as u32;
            self.gpu.config.height = h as u32;
            self.gpu.surface.configure(&self.gpu.device, &self.gpu.config);
            self.last_rect_px = rect;
        }
        // 有效可见 = 播放页激活（弹幕设置卡片迁覆盖层后 modal 隐藏退役——
        // 卡片与 OSC 同层悬浮，覆盖层保持可见与交互）。
        let effective_visible = visible;
        if Some(effective_visible) != self.last_visible {
            show_window(self.hwnd, effective_visible);
            if effective_visible {
                // 恢复显示时重申 z 序：SW_SHOWNOACTIVATE 按当前 z 序显示
                // 不会前置（此前被激活的主窗口压底）。owned 关系本身保证
                // 在 owner 之上，这里是恢复时序上的显式对齐（幂等）。
                bring_to_top(self.hwnd);
            }
            self.last_visible = Some(effective_visible);
        }

        // ---- 即时项（每帧直接生效，不触发重排）----
        // 卡片打开期间（含打开帧/关闭帧）跳过应用：主线程 sync 已按
        // danmaku_settings_open 停止写 run_state，桥内是打开前的静态旧值
        // ——此处若继续应用会把卡片内点击/拖动的即时项（弹幕开关/渐变
        // 开关/渐变范围/透明度/延迟）每帧打回旧值，表现为 check 点击
        // "无响应"（翻转下一帧即回跳）；渐变 check 恒 false 还会联动禁用
        // 渐变范围滑条。关闭帧同样跳过：本地最终值要供关闭沿 export_config
        // 导出；SaveDanmakuConfig 回环（主线程 apply_config + 恢复写入）
        // 后回到正常每帧同步。
        if !(settings_was_open || self.danmaku.settings_open) {
            self.danmaku.enabled = run_state.enabled;
            self.danmaku.show_top_cfg = run_state.show_top;
            self.danmaku.show_bottom_cfg = run_state.show_bottom;
            self.danmaku.opacity = run_state.opacity;
            self.danmaku.delay_seconds = run_state.delay_seconds;
            self.danmaku.gradient_cfg = run_state.gradient;
            self.danmaku.gradient_inset_cfg = run_state.gradient_inset;
        }

        // ---- 配置热同步（布局项）----
        if cfg_version != self.applied_cfg_version {
            let cfg = lock_bridge(&self.bridge).danmaku_cfg.clone();
            self.danmaku.apply_config(&cfg);
            self.applied_cfg_version = cfg_version;
        }

        // ---- 弹幕加载请求 ----
        match load_request {
            Some(OverlayLoad::Auto { video, preferred }) => {
                let xml = self.danmaku.on_file_loaded(&video, preferred.as_deref());
                if let Some(x) = xml {
                    tracing::info!("覆盖层弹幕已加载: {}", x.display());
                }
            }
            Some(OverlayLoad::XmlPath(xml)) => {
                match self.danmaku.load_xml_file(&xml) {
                    Ok(n) => tracing::info!("覆盖层弹幕（手动）已加载 {n} 条: {}", xml.display()),
                    Err(e) => tracing::warn!("覆盖层弹幕手动加载失败: {e}"),
                }
            }
            Some(OverlayLoad::Content(xml)) => {
                match self.danmaku.load_xml_content(&xml) {
                    Ok(n) => tracing::info!("覆盖层弹幕（预取内容）已加载：{n} 条"),
                    Err(e) => tracing::warn!("覆盖层弹幕预取内容加载失败: {e}"),
                }
            }
            Some(OverlayLoad::Clear) => self.danmaku.on_media_change(),
            None => {}
        }

        // ---- OSC 快照读取（主线程每帧写；None = 会话未就绪）+ 跨线程
        //      控制（关菜单/唤醒）+ rfd 结果回程 ----
        let (osc_state, close_track_menu, osc_wake, danmaku_file_result, font_file_result) = {
            let mut b = lock_bridge(&self.bridge);
            let close = b.close_track_menu;
            let wake = b.osc_wake;
            b.close_track_menu = false;
            b.osc_wake = false;
            (
                b.osc_state.clone(),
                close,
                wake,
                b.danmaku_file_result.take(),
                b.font_file_result.take(),
            )
        };
        if close_track_menu && self.osc_ui.track_menu.is_some() {
            self.osc_ui.track_menu = None;
        }
        if osc_wake {
            self.osc_ui.last_activity = std::time::Instant::now();
            self.osc_ui.shown = true;
        }
        // rfd 弹幕文件结果（主线程 PickDanmakuFile 命令的回程）：加载 +
        // manual_pick 经 BindDanmakuFile 命令回主线程落绑定（SW 同语义）。
        if let Some(Some(path)) = danmaku_file_result {
            let res = self.danmaku.load_xml_file(&path);
            if res.is_ok() {
                self.danmaku.manual_pick = Some(path);
            }
            self.danmaku.last_load_result = Some(res);
        }
        // rfd 字体结果：本地校验 + set_font_choice（atlas 就在本线程）。
        if let Some(Some(path)) = font_file_result {
            {
                use dyplayer_danmaku::layout::DanmakuFont;
                if DanmakuFont::from_paths(std::slice::from_ref(&path)).is_some() {
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.danmaku.custom_font_path = Some(path.clone());
                    self.danmaku.set_font_choice(dyplayer_store::DanmakuFontChoice::Custom(
                        path.to_string_lossy().into_owned(),
                    ));
                    self.danmaku.last_font_result = Some(Ok(name));
                } else {
                    self.danmaku.last_font_result = Some(Err(
                        "字体文件无法加载（仅支持 TTF/OTF/TTC）".into(),
                    ));
                }
            }
        }

        // ---- R20B：配置 flush 请求（主线程退出前要最终配置）----
        {
            let mut b = lock_bridge(&self.bridge);
            if b.config_flush_request {
                b.config_flush_request = false;
                b.config_flush_result = Some(self.danmaku.export_config());
            }
        }

        // ---- egui pass：弹幕层 + OSC 层（批 2：OSC 悬浮于视频之上）----
        // R11：不可见时跳过完整绘制——控制任务（加载/配置/owner/即时项）
        // 已在上文处理完，此处只需消息泵（外层 while 循环）保持存活。
        if !effective_visible {
            return;
        }
        let (w, h) = (self.gpu.config.width, self.gpu.config.height);
        if w == 0 || h == 0 {
            return;
        }
        let size_pt = egui::vec2(w as f32 / ppp, h as f32 / ppp);
        // 鼠标事件转译：宿主队列（视频区主通道，host_offset 换算）+
        // 主线程镜像（黑边区直达主窗口 egui 的 pointer 事件）。
        let host_offset = {
            let b = lock_bridge(&self.bridge);
            b.host_offset
        };
        let mut events = take_mouse_events(ppp, host_offset);
        {
            let mut b = lock_bridge(&self.bridge);
            if !b.mouse_events.is_empty() {
                events.append(&mut b.mouse_events);
            }
            if !b.key_events.is_empty() {
                events.append(&mut b.key_events);
            }
        }

        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size_pt)),
            time: Some(self.start.elapsed().as_secs_f64()),
            // 覆盖层虽无 Win32 焦点（NOACTIVATE），但 TextEdit 事件处理只查
            // memory.has_focus(id)（builder.rs:654），不查此字段。设 true 让
            // 光标 blink（builder.rs:1015）与 Response::has_focus()（样式）正常。
            focused: true,
            events,
            ..Default::default()
        };
        self.gpu.ctx.begin_pass(raw);
        // 独立 ctx 的根 Ui：egui 0.37 Panel 系只能在父 Ui 上展开，手动
        // UiBuilder 构造（layer 落 background，end_pass 收集 shapes 走
        // 常规管线）。
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size_pt);
        let ui_builder = egui::UiBuilder::new().max_rect(screen_rect);
        let mut root_ui = egui::Ui::new(
            self.gpu.ctx.clone(),
            egui::Id::unique("overlay-root"),
            ui_builder,
        );
        // R12-B1：收取后台 XML 加载结果（与 SW 路径 player_app.rs:2853 对称；
        // on_file_loaded 异步化后覆盖层实例同样需每帧 poll，否则
        // pending_xml_load 永滞 → session 永空 → 弹幕不显示）。
        self.danmaku.poll_xml_load();
        self.danmaku.render(&mut root_ui, screen_rect, media_time);
        // OSC（一套代码两处运行：见 osc_state.rs 契约）。命令先入本地
        // 队列，pass 结束后统一写桥（防止 draw 中途锁桥）。
        let mut osc_cmds = Vec::new();
        if let Some(st) = &osc_state {
            crate::player_app::osc::draw(st, &mut self.osc_ui, &mut root_ui, &mut osc_cmds);
        }
        // 弹幕设置卡片（迁覆盖层：悬浮于视频之上，复刻 SW 观感；rfd 经
        // 命令桥移交主线程——defer_file_dialogs 模式）。
        if self.danmaku.settings_open
            && let Some(st) = &osc_state
        {
            let theme =
                crate::player_app::osc::THEMES[st.theme_idx.min(crate::player_app::osc::THEMES.len() - 1)];
            let anchor = self
                .osc_ui
                .danmaku_btn_rect
                .map(|r| egui::pos2(r.center().x, r.top() - 6.0));
            self.danmaku.show_settings(&mut root_ui, anchor, theme);
        }
        // defer 模式的文件选择请求转命令（主线程 rfd）。
        if self.danmaku.pending_pick {
            self.danmaku.pending_pick = false;
            osc_cmds.push(crate::osc_state::OscCmd::PickDanmakuFile);
        }
        if self.danmaku.pending_font_pick {
            self.danmaku.pending_font_pick = false;
            osc_cmds.push(crate::osc_state::OscCmd::PickDanmakuFont);
        }
        // 卡片关闭沿：导出配置落盘（主线程 apply + save + push 回环）。
        if settings_was_open && !self.danmaku.settings_open {
            osc_cmds.push(crate::osc_state::OscCmd::SaveDanmakuConfig(
                self.danmaku.export_config(),
            ));
        }
        // 卡片内手动加载成功的弹幕文件 → 主线程 bind_danmaku。
        if let Some(xml) = self.danmaku.manual_pick.take() {
            osc_cmds.push(crate::osc_state::OscCmd::BindDanmakuFile(xml));
        }
        let mut output = self.gpu.ctx.end_pass();
        // R19：检查覆盖层焦点状态（主线程据此跳过播放器快捷键）。
        let overlay_has_focus = self.gpu.ctx.memory(|m| m.focused().is_some());
        let clipped = self.gpu.ctx.tessellate(output.shapes, ppp);
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [w, h],
            pixels_per_point: ppp,
        };

        // ---- 纹理增量（字形 atlas 首帧/重建；0.37 的 delta.set 是
        // SmallVec< ImageDelta >——一个 id 可多段更新）。处理完必须
        // clear()：debug 下 TexturesDelta drop 断言未消费 delta（AGENTS.md
        // 既有坑：跑空 pass 吸收 delta 的同族问题）。----
        for (id, image_deltas) in &output.textures_delta.set {
            for delta in image_deltas {
                self.gpu
                    .renderer
                    .update_texture(&self.gpu.device, &self.gpu.queue, *id, delta);
            }
        }
        let free_ids: Vec<_> = output.textures_delta.free.iter().copied().collect();

        // ---- OSC 命令/锚点/设置开态回写主线程（R05：业务提交不依赖
        // present 成功，提前到取帧之前，失败帧也交付已消费命令/状态）。----
        {
            if let Ok(mut b) = self.bridge.lock() {
                if !osc_cmds.is_empty() {
                    b.osc_cmds.extend(osc_cmds);
                }
                b.overlay_has_focus = overlay_has_focus;
                b.osc_btn_rect = self.osc_ui.danmaku_btn_rect;
                if settings_was_open && !self.danmaku.settings_open {
                    b.run_state = OverlayRunState {
                        enabled: self.danmaku.enabled,
                        show_top: self.danmaku.show_top_cfg,
                        show_bottom: self.danmaku.show_bottom_cfg,
                        opacity: self.danmaku.opacity,
                        delay_seconds: self.danmaku.delay_seconds,
                        gradient: self.danmaku.gradient_cfg,
                        gradient_inset: self.danmaku.gradient_inset_cfg,
                    };
                    b.run_state_hold = true;
                    b.danmaku_delay = Some(self.danmaku.delay_seconds);
                }
                b.danmaku_settings_open = self.danmaku.settings_open;
            }
        }

        // ---- 取帧 + 渲染 ----
        let frame = match self.gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.gpu
                    .surface
                    .configure(&self.gpu.device, &self.gpu.config);
                for id in &free_ids {
                    self.gpu.renderer.free_texture(id);
                }
                output.textures_delta.clear();
                return;
            }
            _ => {
                for id in &free_ids {
                    self.gpu.renderer.free_texture(id);
                }
                output.textures_delta.clear();
                return; // Timeout/Occluded/Validation：跳帧
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("overlay-enc"),
            });
        let prep = self.gpu.renderer.update_buffers(
            &self.gpu.device,
            &self.gpu.queue,
            &mut encoder,
            &clipped,
            &screen_desc,
        );
        {
            let rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // PreMultiplied：alpha=0 全透明透出视频
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // forget_lifetime：renderer 签名要求 RenderPass<'static>（egui_wgpu
            // 官方同款用法——pass 未越过 encoder 生命周期，运行时安全）。
            self.gpu
                .renderer
                .render(&mut rp.forget_lifetime(), &clipped, &screen_desc);
        }
        self.gpu
            .queue
            .submit(prep.into_iter().chain(std::iter::once(encoder.finish())));
        self.gpu.queue.present(frame);
        for id in &free_ids {
            self.gpu.renderer.free_texture(id);
        }
        output.textures_delta.clear();


        // ---- 帧率统计 ----
        self.frame_count += 1;
        if self.danmaku.last_visible > 0 {
            self.dm_frame_counter += 1;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.last_fps_sample).as_secs_f64();
        if dt >= 0.5 {
            self.fps = self.frame_count as f64 / dt;
            self.dm_fps = self.dm_frame_counter as f64 / dt;
            self.frame_count = 0;
            self.dm_frame_counter = 0;
            self.last_fps_sample = now;
        }
    }
}

unsafe fn get_module_handle() -> usize {
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

fn create_overlay_window(rect: [i32; 4], visible: bool) -> anyhow::Result<usize> {
    use std::sync::OnceLock;
    static ONCE: OnceLock<bool> = OnceLock::new();
    let registered = *ONCE.get_or_init(|| unsafe {
        let inst = get_module_handle();
        if inst == 0 {
            return false;
        }
        let class_name = wide(CLASS_NAME);
        let wc = WndClassW {
            // CS_DBLCLKS：双击消息（OSC 点击层的双击=播放/暂停语义与 SW 一致）。
            style: 0x0008,
            proc: overlay_wnd_proc as *const () as usize,
            cls_extra: 0,
            wnd_extra: 0,
            inst,
            icon: 0,
            cursor: 0,
            background: 0, // NOREDIRECTIONBITMAP：无 redirection surface，不设刷
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
        };
        RegisterClassW(&wc) != 0
    });
    anyhow::ensure!(registered, "RegisterClassW({CLASS_NAME}) 失败");

    let class_name = wide(CLASS_NAME);
    let title = wide("DyPlayer danmaku overlay");
    let (w, h) = ((rect[2] - rect[0]).max(1), (rect[3] - rect[1]).max(1));
    let style = if visible { WS_POPUP | WS_VISIBLE } else { WS_POPUP };
    // NOREDIRECTIONBITMAP：无 DWM 重定向 surface（透明 DComp 窗口的标准
    // 形态——与 winit with_transparent 同款），swapchain 必须 DxgiFromVisual。
    // **不带 WS_EX_LAYERED**：layered 窗口的 hit test 按像素 alpha 穿透
    //（alpha=0 区直接命中下层宿主——实测鼠标永远到不了覆盖层）；DComp
    // visual 透明混合与 LAYERED 无关，只靠 NOREDIRECTIONBITMAP。批 2 起
    // 覆盖层接管播放区交互（鼠标 → egui input → osc::draw 命令桥）。
    let ex = WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP;
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            rect[0],
            rect[1],
            w,
            h,
            0,
            0,
            get_module_handle(),
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(hwnd != 0, "CreateWindowExW 失败（gle={:?}）", std::io::Error::last_os_error());
    Ok(hwnd)
}

fn init_overlay_gpu(hwnd: usize, w: u32, h: u32) -> anyhow::Result<OverlayGpu> {
    // 手工 RawHandle（winit 不可用，见模块文档）。
    // raw-window-handle 0.6：Win32 句柄是 NonZero<isize>
    let hwnd_nz = std::num::NonZeroIsize::new(hwnd as isize)
        .ok_or_else(|| anyhow::anyhow!("hwnd 为空"))?;
    let window_handle =
        raw_window_handle::RawWindowHandle::Win32(raw_window_handle::Win32WindowHandle::new(
            hwnd_nz,
        ));
    let display_handle = raw_window_handle::RawDisplayHandle::Windows(
        raw_window_handle::WindowsDisplayHandle::new(),
    );
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        // 透明窗口必须 DComp visual swapchain（PoC-2 坑）
        backend_options: wgpu::BackendOptions {
            dx12: wgpu::Dx12BackendOptions {
                presentation_system: wgpu::Dx12SwapchainKind::DxgiFromVisual,
                ..Default::default()
            },
            ..Default::default()
        },
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(display_handle),
            raw_window_handle: window_handle,
        })?
    };
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("danmaku-overlay"),
        ..Default::default()
    }))?;

    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .or_else(|| caps.formats.first().copied())
        .ok_or_else(|| anyhow::anyhow!("覆盖层 surface 无可用格式"))?;
    let alpha_mode = if caps
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        anyhow::bail!("覆盖层 surface 不支持 PreMultiplied");
    };
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: w,
        height: h,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode,
        view_formats: Vec::new(),
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);
    let renderer = egui_wgpu::Renderer::new(
        &device,
        format,
        egui_wgpu::RendererOptions {
            // 弹幕 mesh 无文字/无 dithering 需求；MSAA 关（字形边缘由
            // atlas 光栅化抗锯齿承担）
            msaa_samples: 1,
            dithering: false,
            ..Default::default()
        },
    );
    let ctx = egui::Context::default();
    // 覆盖层 ctx 必须装 CJK 字体链：osc_font 用 FontFamily::Name("HarmonyOS
    // Sans SC")——Name family 未注册时 epaint 返回空链 → OSC 文字全豆腐
    //（AGENTS.md 字体坑；弹幕自带 atlas 不受影响，但 OSC 走 ctx 字体）。
    crate::player_app::install_cjk_font(&ctx, &dyplayer_store::FontChoice::App);
    // SVG 图标（OSC 媒体库胶囊 house）：egui_extras loader 是 ctx 级——
    // 覆盖层 ctx 不装则 Image 加载失败（图标空白）。幂等安装。
    crate::player_app::osc::install_svg_icons(&ctx);
    Ok(OverlayGpu {
        device,
        queue,
        surface,
        config,
        renderer,
        ctx,
    })
}

// ---- Win32 小工具（SetWindowPos / ShowWindow 经 windows crate 走类型安全路径）----

fn move_window(hwnd: usize, x: i32, y: i32, w: i32, h: i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOZORDER,
    };
    unsafe {
        let _ = SetWindowPos(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
            None,
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_NOCOPYBITS | SWP_NOZORDER,
        );
    }
}

/// 挂接 owner（GWLP_HWNDPARENT）：owned window 语义保证覆盖层永远在
/// owner（主窗口）之上、随其最小化、不盖其他应用。只改 owner 关系，
/// 不改变坐标系/子窗口属性（与 SetParent 语义无关）。
fn set_window_owner(hwnd: usize, owner: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_HWNDPARENT};
    unsafe {
        let _ = SetWindowLongPtrW(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
            GWLP_HWNDPARENT,
            owner,
        );
    }
}

/// 重申 z 序到非 topmost 组顶（HWND_TOP；不带 SWP_NOZORDER）。
/// 修"激活主窗口把覆盖层压底"：NOACTIVATE 窗口永不被激活，主窗口每次
/// 激活都会被提到非 topmost 组顶端。owned 关系是长期保证，此处是时序
/// 补救（owner 挂接后 / 恢复显示后各一次，幂等）。
fn bring_to_top(hwnd: usize) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_TOP, SWP_NOACTIVATE};
    unsafe {
        let _ = SetWindowPos(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
        );
    }
}

fn show_window(hwnd: usize, visible: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE};
    unsafe {
        let _ = ShowWindow(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
            if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
        );
    }
}