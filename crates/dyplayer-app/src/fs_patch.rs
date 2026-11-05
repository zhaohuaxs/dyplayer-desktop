//! Windows 窗口样式自管（常态无边框 + 自管全屏）。
//!
//! 全屏冻结修复（根因实验确证 2026-10-18）：winit 全屏把窗口切到无装饰样式
//! （`WS_VISIBLE|WS_CLIPSIBLINGS|WS_CLIPCHILDREN`，无边框位）后，本机驱动/DWM
//! 组合（GameViewer 虚拟显示适配器 + Intel UHD 混合）下 WGL `SwapBuffers` 虽
//! 每帧成功，帧内容却不再进入 DWM 重定向 surface——渲染管线完整运转，屏幕停在
//! 旧帧（画面/弹幕冻结），DXGI 桌面复制同步断流；且冻结一旦发生不可逆（仅恢复
//! 装饰样式可解除）。
//!
//! 样式矩阵实验：从健康装饰态**一步**切到"无 WS_CAPTION 但保留
//! `WS_THICKFRAME|WS_SYSMENU`"的全屏窗口，DWM 合成路径正常。
//!
//! 常态无边框（2026-10-24）：`with_decorations(false)`（winit 剥光样式）在
//! Win10 虚拟机（VMware SVGA3D）上启动即客户区全黑、无任何元素渲染——最小
//! wgpu demo 同机复现坐实（`tmp_verify/wgpu_counter`：默认装饰正常 /
//! `--no-decorations` 全黑 / `--borderless-patch` THICKFRAME+NCCALCSIZE 正常）。
//! 因此主窗口不再经 winit 去装饰：正常装饰创建 + 首帧 `prime_borderless`
//! 切成 THICKFRAME 无边框样式。拖动/缩放/最大化交互全部走 egui 层
//! （titlebar.rs 的 StartDrag/BeginResize/Maximized），子类只负责
//! `WM_NCCALCSIZE`→0（客户区=外框）与 `WM_NCHITTEST`→HTCLIENT（边缘
//! resize 由 titlebar.rs 唯一负责，防系统/egui 双路径打架）。
//!
//! 全屏进入 = 保存 `WINDOWPLACEMENT` + 样式一步切 THICKFRAME 版 + 盖满所在
//! 显示器（子类安装幂等——常态已装则不重装，防叠层）；退出 = 恢复样式
//! （即常态无边框样式）与原位置/最大化状态。`DwmSetWindowAttribute(
//! DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE)` 去掉 DWM 绘制的 1px 边框
//! （Win11+；失败则忽略）。

#![cfg_attr(not(feature = "backend-wgpu"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWL_STYLE, GWLP_WNDPROC, GetWindowLongW, GetWindowPlacement, IsZoomed,
    SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowLongW,
    SetWindowPlacement, SetWindowPos, ShowWindow, WINDOWPLACEMENT, WNDPROC, WS_BORDER, WS_CAPTION,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, SW_SHOW,
    WS_THICKFRAME, WS_VISIBLE,
};

const WM_NCCALCSIZE: u32 = 0x0083;
const WM_NCHITTEST: u32 = 0x0084;
const WM_ERASEBKGND: u32 = 0x0014;
const HTCLIENT: isize = 1;

/// 无标题栏的 THICKFRAME 常态样式（DWM 合成路径健康）。
/// 保留 MIN/MAXIMIZEBOX 供系统手势（Aero Snap、双击）与 egui Maximized 命令。
const BORDERLESS_WINDOWED_STYLE: u32 = WS_VISIBLE.0
    | WS_CLIPSIBLINGS.0
    | WS_CLIPCHILDREN.0
    | WS_SYSMENU.0
    | WS_THICKFRAME.0
    | WS_MINIMIZEBOX.0
    | WS_MAXIMIZEBOX.0;

/// 无标题栏的 THICKFRAME 全屏样式（DWM 合成路径健康）。
const BORDERLESS_FULLSCREEN_STYLE: u32 =
    WS_VISIBLE.0 | WS_CLIPSIBLINGS.0 | WS_CLIPCHILDREN.0 | WS_SYSMENU.0 | WS_THICKFRAME.0;

static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);
static SUBCLASS_INSTALLED: AtomicBool = AtomicBool::new(false);
/// 全屏态标志（enter/exit 维护）：maintain_borderless 不得在全屏期间把
/// 样式"自愈"成常态版（全屏样式由 FullscreenGuard 生命周期管理）。
static FULLSCREEN: AtomicBool = AtomicBool::new(false);
/// 主窗口 HWND 缓存（prime_borderless 首帧写入，maintain_borderless 复用）。
static MAIN_HWND: OnceLock<isize> = OnceLock::new();

unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // 客户区 = 整窗外框（去掉标题栏/边框占位）。返回 WVR_REDRAW(1) 让我们设置的
    // rgrc[0] 生效；返回 0 会让 Windows 按当前样式默认计算（剥 CAPTION 后仍扣
    // THICKFRAME 的 sizing border），导致 GetClientRect != 外框，winit 缓存的
    // inner_position 与实际客户区左上角偏离一个 caption 高度 → 鼠标命中位置整体
    // 上移（按钮要在下方才能点中，Win10 尤其明显）。
    //
    // 非最大化：rgrc[0] 即 Windows 传入的外框矩形，不动 → 客户区 = 外框。
    // 最大化：THICKFRAME 窗口被系统 overscreen（外框比屏幕大一圈），须把客户区
    // 收回工作区防边缘被裁。
    if msg == WM_NCCALCSIZE && wp.0 != 0 {
        if unsafe { IsZoomed(hwnd) }.as_bool() {
            unsafe {
                let rgrc = lp.0 as *mut RECT;
                let monitor: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    (*rgrc).left = info.rcWork.left;
                    (*rgrc).top = info.rcWork.top;
                    (*rgrc).right = info.rcWork.right;
                    (*rgrc).bottom = info.rcWork.bottom;
                }
            }
        }
        return LRESULT(1); // WVR_REDRAW
    }
    // 阻止系统擦背景（Win32 类默认白刷 → 隐藏创建显示瞬间回闪白）；egui
    // 每帧完整 present 覆盖客户区，不依赖 WM_PAINT 擦除。
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }

    // 边缘 resize 命中统一交给 egui 层（titlebar.rs resize_edges → BeginResize），
    // 防系统命中与 egui 命中双路径打架；鼠标事件全部直达客户区。
    if msg == WM_NCHITTEST {
        return LRESULT(HTCLIENT);
    }
    let orig = ORIG_WNDPROC.load(Ordering::Relaxed);
    if orig == 0 {
        return LRESULT(0);
    }
    unsafe {
        let proc: WNDPROC = std::mem::transmute(orig);
        CallWindowProcW(proc, hwnd, msg, wp, lp)
    }
}

fn set_dwm_border_color(hwnd: HWND, color: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// 装子类（幂等——只装一次，防叠层：常态无边框与全屏共用同一子类）。
unsafe fn ensure_subclass(hwnd: HWND) {
    if SUBCLASS_INSTALLED.swap(true, Ordering::Relaxed) {
        return;
    }
    let orig = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass_proc as *const () as isize) };
    if orig != 0 {
        ORIG_WNDPROC.store(orig, Ordering::Relaxed);
    }
}

/// 常态无边框安装：装饰窗口创建后切 THICKFRAME 无边框样式（不动位置尺寸）。
pub fn install_borderless(hwnd: HWND) {
    unsafe {
        ensure_subclass(hwnd);
        fixup_borderless_style(hwnd);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        set_dwm_border_color(hwnd, DWMWA_COLOR_NONE);
    }
}

/// 位修正：只剥装饰位（CAPTION|BORDER）、补无边框必备位（THICKFRAME 等），
/// **保留其余位**——关键保留 `WS_MAXIMIZE`：winit `set_maximized` 会用内部
/// 快照重写整个 style（基底恒含 WS_CAPTION），若用写死常量覆盖会把
/// WS_MAXIMIZE 一并抹掉 → 窗口闪回还原 + WM_SIZE(SIZE_RESTORED) 清 winit
/// MAXIMIZED 标志 → egui maximized 恒 false（双击/按钮一直重复最大化）。
unsafe fn fixup_borderless_style(hwnd: HWND) {
    unsafe {
        let cur = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let fixed = borderless_of(cur);
        if fixed != cur {
            SetWindowLongW(hwnd, GWL_STYLE, fixed as i32);
        }
    }
}

/// 从当前 style 推导无边框样式：剥装饰位、补必备位，其余位（WS_MAXIMIZE 等）原样保留。
fn borderless_of(cur: u32) -> u32 {
    (cur & !(WS_CAPTION.0 | WS_BORDER.0)) | BORDERLESS_WINDOWED_STYLE
}

/// ui() 首帧调用（仅首次生效）：把 winit 装饰窗口切成自管无边框。
/// 替代 `with_decorations(false)`（该路径在 Win10 虚拟机 DWM 呈现失效 → 全黑）。
pub fn prime_borderless(frame: &eframe::Frame) {
    static PRIMED: OnceLock<()> = OnceLock::new();
    if PRIMED.get().is_some() {
        return;
    }
    let _ = PRIMED.set(());
    use raw_window_handle::HasWindowHandle;
    if let Ok(handle) = frame.window_handle()
        && let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        let hwnd = HWND(win32.hwnd.get() as *mut core::ffi::c_void);
        let _ = MAIN_HWND.set(hwnd.0 as isize);
        #[cfg(feature = "backend-wgpu")]
        {
            install_borderless(hwnd);
            tracing::info!("BORDERLESS: 常态无边框已安装（THICKFRAME+NCCALCSIZE，替代 with_decorations(false)）");
        }
    }
}

/// ui() 每帧调用：校验窗口样式未被外方改写（winit 启动风暴/后续事件中的
/// apply_window_flags 会把 WS_CAPTION 写回——首帧安装实测被冲掉），被改写
/// 则恢复 + FRAMECHANGED。无变化时开销仅一次 GetWindowLongW（微秒级）。
/// 全屏期间跳过（样式由 FullscreenGuard 管理）。
pub fn maintain_borderless() {
    #[cfg(feature = "backend-wgpu")]
    {
        if FULLSCREEN.load(Ordering::Relaxed) {
            return;
        }
        let Some(hwnd) = MAIN_HWND.get().map(|h| HWND(*h as *mut core::ffi::c_void)) else {
            return;
        };
        let cur = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        if cur & WS_CAPTION.0 != 0 || cur & WS_THICKFRAME.0 == 0 {
            unsafe {
                SetWindowLongW(hwnd, GWL_STYLE, borderless_of(cur) as i32);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
                );
                set_dwm_border_color(hwnd, DWMWA_COLOR_NONE);
            }
            tracing::warn!("BORDERLESS: 窗口样式被外方改写（winit refresh），已自愈");
        }
    }
}


/// 强制触发 WM_NCCALCSIZE 重算客户区。
/// glow 后端首帧可能不触发 WM_NCCALCSIZE（GL 上下文初始化路径与 wgpu 不同），
/// 导致标题栏高度未初始化；ui 首帧（显示后）调一次确保 DWM 参与后重算。
#[cfg_attr(feature = "backend-wgpu", allow(dead_code))]
pub fn force_ncalc() {
    let Some(hwnd) = MAIN_HWND.get().map(|h| HWND(*h as *mut core::ffi::c_void)) else {
        return;
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

/// 隐藏创建（with_visible(false)）后的首次显示：直接 Win32 `ShowWindow`。
/// 不走 egui `ViewportCommand::Visible` 命令链——实测经命令链在事件循环
/// 调度下延迟秒级才生效（winit/eframe 对不可见窗口的 repaint 节流相关），
/// Win32 直调即时。调用前先 [`prime_borderless`]（显示前样式就绪）。
pub fn show_main_window() {
    if let Some(h) = MAIN_HWND.get() {
        unsafe {
            let _ = ShowWindow(HWND(*h as *mut core::ffi::c_void), SW_SHOW);
        }
        tracing::info!("BORDERLESS: 隐藏创建完成，窗口已显示（无白闪启动）");
    }
}

/// 退出全屏时恢复窗口原状所需的状态。
pub struct FullscreenGuard {
    hwnd: isize,
    placement: WINDOWPLACEMENT,
    style: i32,
}

fn monitor_rect(hwnd: HWND) -> RECT {
    unsafe {
        let monitor: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcMonitor
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        }
    }
}

/// 进入自管全屏：装 NCCALCSIZE/NCHITTEST 拦截（**必须先于样式切换**——
/// 否则 FRAMECHANGED 触发的 WM_NCCALCSIZE 会先落到 winit 的处理上：
/// 其 is_maximized 分支把客户区设为工作区 rcWork（扣任务栏），导致
/// egui viewport 比屏幕矮、视频 letterbox），样式一步切 THICKFRAME 版
/// （不经过无装饰冻结态），盖满所在显示器。
pub fn enter_fullscreen(hwnd: HWND) -> FullscreenGuard {
    unsafe {
        FULLSCREEN.store(true, Ordering::Relaxed);
        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        let _ = GetWindowPlacement(hwnd, &mut placement);
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let rect = monitor_rect(hwnd);
        ensure_subclass(hwnd);
        SetWindowLongW(hwnd, GWL_STYLE, BORDERLESS_FULLSCREEN_STYLE as i32);
        let _ = SetWindowPos(
            hwnd,
            None,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        set_dwm_border_color(hwnd, DWMWA_COLOR_NONE);
        FullscreenGuard {
            hwnd: hwnd.0 as isize,
            placement,
            style,
        }
    }
}

/// 退出全屏：卸样式恢复（常态即无边框样式——子类保持安装），恢复原
/// 位置/最大化状态。边框色保持 NONE（常态无边框与安装态一致）。
pub fn exit_fullscreen(guard: FullscreenGuard) {
    let hwnd = HWND(guard.hwnd as *mut core::ffi::c_void);
    FULLSCREEN.store(false, Ordering::Relaxed);
    unsafe {
        set_dwm_border_color(hwnd, DWMWA_COLOR_NONE);
        SetWindowLongW(hwnd, GWL_STYLE, guard.style);
        let _ = SetWindowPlacement(hwnd, &guard.placement);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOZORDER | SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    /// 常态无边框样式：保留 THICKFRAME（DWM 合成健康的关键位）、去 CAPTION，
    /// 且带 MIN/MAXIMIZEBOX（系统手势 + egui Maximized 命令可用）。
    #[test]
    fn windowed_style_keeps_thickframe_drops_caption() {
        assert_eq!(BORDERLESS_WINDOWED_STYLE & WS_THICKFRAME.0, WS_THICKFRAME.0);
        assert_eq!(BORDERLESS_WINDOWED_STYLE & WS_CAPTION.0, 0);
        assert_eq!(BORDERLESS_WINDOWED_STYLE & WS_MINIMIZEBOX.0, WS_MINIMIZEBOX.0);
        assert_eq!(BORDERLESS_WINDOWED_STYLE & WS_MAXIMIZEBOX.0, WS_MAXIMIZEBOX.0);
        assert_eq!(BORDERLESS_WINDOWED_STYLE & WS_SYSMENU.0, WS_SYSMENU.0);
    }

    /// 全屏样式：同样保留 THICKFRAME（无装饰冻结根因位不回退）。
    #[test]
    fn fullscreen_style_keeps_thickframe() {
        assert_eq!(
            BORDERLESS_FULLSCREEN_STYLE & WS_THICKFRAME.0,
            WS_THICKFRAME.0
        );
        assert_eq!(BORDERLESS_FULLSCREEN_STYLE & WS_CAPTION.0, 0);
    }

    /// 两种样式仅在 MIN/MAXIMIZEBOX 上不同（全屏不需要系统盒）。
    #[test]
    fn styles_differ_only_in_sysboxes() {
        assert_eq!(
            BORDERLESS_WINDOWED_STYLE & !WS_MINIMIZEBOX.0 & !WS_MAXIMIZEBOX.0,
            BORDERLESS_FULLSCREEN_STYLE
        );
    }

    /// 位修正语义：剥装饰位（CAPTION|BORDER）、补必备位、**保留其余位**
    /// —— winit set_maximized 重写的 style 带 CAPTION|WS_MAXIMIZE，自愈若用
    /// 写死常量会抹 WS_MAXIMIZE → 窗口闪回还原 + winit MAXIMIZED 标志被
    /// WM_SIZE(SIZE_RESTORED) 清掉（双击/按钮重复最大化 bug，实测定位）。
    #[test]
    fn borderless_of_preserves_maximize_bit_and_strips_caption() {
        use windows::Win32::UI::WindowsAndMessaging::WS_MAXIMIZE;
        // winit 最大化时写入的典型 style：BORDERLESS + CAPTION + WS_MAXIMIZE
        let winit_maximized = BORDERLESS_WINDOWED_STYLE | WS_CAPTION.0 | WS_MAXIMIZE.0;
        let fixed = borderless_of(winit_maximized);
        assert_eq!(fixed & WS_CAPTION.0, 0, "必须剥掉 CAPTION");
        assert_eq!(
            fixed & WS_MAXIMIZE.0,
            WS_MAXIMIZE.0,
            "必须保留 WS_MAXIMIZE（否则最大化被自愈抹掉）"
        );
        assert_eq!(fixed & WS_THICKFRAME.0, WS_THICKFRAME.0);
        // 还原态：无 WS_MAXIMIZE 输入 → 输出也不引入
        assert_eq!(borderless_of(BORDERLESS_WINDOWED_STYLE | WS_CAPTION.0) & WS_MAXIMIZE.0, 0);
    }
}
