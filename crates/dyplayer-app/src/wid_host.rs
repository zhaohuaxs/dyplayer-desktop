//! mpv `wid` 自管渲染的视频宿主窗口（M1，见 docs/poc-wid-gpu-render.md）。
//!
//! GPU 渲染路径（`vo=gpu-next` + `wid`）下 mpv 在指定 HWND 内自建 D3D11
//! swapchain 呈现视频，与 eframe/wgpu 上下文零耦合（渲染解耦红线允许的
//! 替代路径）。宿主窗口生命周期：
//!
//! 1. `create_hidden`：会话构造时先建**隐藏顶级窗口**（mpv wid 须在
//!    initialize 前设置，而此刻 eframe 主窗口句柄尚未传给会话）；
//! 2. `attach`：ui() 首帧从 eframe::Frame 拿主窗口 HWND，SetParent 重父 +
//!    样式一步切 `WS_CHILD|WS_VISIBLE|WS_CLIPSIBLINGS`；
//! 3. `align`：每帧把宿主窗口对齐 egui 计算的视频矩形（物理像素，变化
//!    ≥1px 才提交 SetWindowPos），mpv 自行处理 aspect letterbox；
//! 4. Drop：销毁窗口（PlayerSession 显式在 mpv client 之后释放）。
//!
//! 已知限制（M1，M2 覆盖窗口解决）：宿主窗口盖住 egui 中央视频区，弹幕
//! 与 OSC 在视频矩形内的部分被遮挡；键盘快捷键与视频区外 UI（侧栏设置
//! 面板等）不受影响。

use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetForegroundWindow,
    SetParent, SetWindowLongPtrW, SetWindowPos, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, GWL_STYLE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOZORDER, WINDOW_STYLE, WNDCLASSW,
    WS_CHILD, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE, WS_POPUP, WS_VISIBLE,
};
use windows::core::{w, PCWSTR};

/// mpv wid 宿主窗口句柄封装。
pub struct VideoHost {
    hwnd: HWND,
    /// 最近一次对齐的物理像素 rect（l/t/r/b）；None = 尚未对齐/强制重对齐。
    last_rect_px: Option<[i32; 4]>,
}

/// 逻辑坐标 rect → 客户区物理像素 rect（left/top/right/bottom）。
/// 取整用 round：与 egui 层 texel 对齐策略一致，避免逐帧亚像素抖动。
pub fn to_px_rect(rect: egui::Rect, ppp: f32) -> [i32; 4] {
    let l = (rect.left() * ppp).round() as i32;
    let t = (rect.top() * ppp).round() as i32;
    let r = (rect.right() * ppp).round() as i32;
    let b = (rect.bottom() * ppp).round() as i32;
    [l, t, r.max(l), b.max(t)]
}


fn register_class_once() -> bool {
    static ONCE: OnceLock<bool> = OnceLock::new();
    *ONCE.get_or_init(|| unsafe {
        let Some(inst) = GetModuleHandleW(PCWSTR::null()).ok() else {
            return false;
        };
        let wc = WNDCLASSW {
            // CS_DBLCLKS：双击消息（视频区双击=播放/暂停，覆盖层 OSC
            // 点击层消费——egui double_clicked 判定依赖第二次 down）。
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hInstance: inst.into(),
            hbrBackground: HBRUSH(GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH).0),
            lpszClassName: w!("DyPlayerWidHost"),
            ..Default::default()
        };
        RegisterClassW(&wc) != 0
    })
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // 视频区鼠标（宿主是真实命中窗口——DComp 覆盖层 alpha=0 区穿透
    // 到此）：直接写入弹幕覆盖层的鼠标队列（坐标 = 宿主客户区原样，
    // 覆盖线程经桥的 host_offset 换算成自己的逻辑坐标）。**不能**
    // PostMessage 主窗口——winit 0.30 忽略注入消息（AGENTS.md 既有坑，
    // 实测 egui 事件流零输出）。
    if crate::overlay_window::push_host_mouse_if_overlay(msg, wp.0, lp.0) {
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl VideoHost {
    /// 创建屏幕外可见窗口（mpv wid 目标）。
    /// 必须带 WS_VISIBLE：vo=gpu-next 的 vsync/呈现初始化依赖 DWM 参与，
    /// 隐藏窗口会让 display-resample 配速下的 initialize 死等（实测 hang）；
    /// 屏幕外坐标（-32000）保证用户桌面不可见。
    /// WS_EX_NOACTIVATE：创建可见顶级窗口时不抢前台/键盘焦点（否则 Browse
    /// 双击进入后宿主抢走 eframe 主窗口前台，attach 的 SetParent 转子窗口
    /// 后前台丢失，winit 收不到 WM_KEYDOWN → 键盘快捷键失灵）。
    pub fn create_hidden() -> Option<VideoHost> {
        if !register_class_once() {
            return None;
        }
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("DyPlayerWidHost"),
                w!("DyPlayer mpv host"),
                WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0),
                -32000,
                -32000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .ok()?
        };
        Some(VideoHost {
            hwnd,
            last_rect_px: None,
        })
    }

    /// mpv wid 需要的窗口句柄整数值。
    pub fn hwnd(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// 重父进 eframe 主窗口并转为可见子窗口（每会话一次）。
    /// SetParent 后子窗口坐标系切换为客户区相对——随后必须 `align`。
    /// parent 同时登记为鼠标转发目标（视频区交互回主窗口 egui）。
    pub fn attach(&mut self, parent: HWND) {
        unsafe {
            let style = (WS_CHILD.0 | WS_VISIBLE.0 | WS_CLIPSIBLINGS.0) as isize;
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, style);
            let _ = SetParent(self.hwnd, Some(parent));
            // FRAMECHANGED 让新样式生效；尺寸交由随后的 align 设置。
            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOCOPYBITS | SWP_NOZORDER,
            );
            // SetParent 把宿主从顶级转子窗口，若此前宿主持前台则丢失；
            // 显式回拉 eframe 主窗口前台+键盘焦点，保证 winit 收到 WM_KEYDOWN。
            let _ = SetForegroundWindow(parent);
        }
        self.last_rect_px = None;
    }

    /// 对齐视频矩形（物理像素 l/t/r/b；变化才提交）。
    pub fn align(&mut self, rect_px: [i32; 4]) {
        if self.last_rect_px == Some(rect_px) {
            return;
        }
        let x = rect_px[0];
        let y = rect_px[1];
        let w = (rect_px[2] - rect_px[0]).max(1);
        let h = (rect_px[3] - rect_px[1]).max(1);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOCOPYBITS | SWP_NOZORDER,
            );
        }
        self.last_rect_px = Some(rect_px);
    }

    /// 移到屏幕外并作废对齐缓存（离开播放页时收走宿主窗口）。
    /// 用屏幕外（-32000）而非 SW_HIDE：与创建策略一致——mpv swapchain
    /// 的呈现依赖窗口参与 DWM（隐藏窗口有死等风险，AGENTS.md 坑 2）；
    /// 暂停态下 mpv 不产帧，成本为零。缓存作废保证下次 `align` 必然
    /// 提交（"变化才提交"不会吞掉回位）。转发偏移同步移出屏幕（park
    /// 期间残留的鼠标事件转发到屏幕外坐标，主窗口视为界外忽略）。
    pub fn park_offscreen(&mut self) {
        if self.last_rect_px.is_none() {
            // 已在屏幕外（park 幂等）。
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                -32000,
                -32000,
                1,
                1,
                SWP_NOACTIVATE | SWP_NOCOPYBITS | SWP_NOZORDER,
            );
        }
        self.last_rect_px = None;
    }
}

impl Drop for VideoHost {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_rect_scales_by_pixels_per_point() {
        let rect = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(100.0, 200.0));
        assert_eq!(to_px_rect(rect, 1.0), [10, 20, 100, 200]);
        assert_eq!(to_px_rect(rect, 1.5), [15, 30, 150, 300]);
    }

    #[test]
    fn px_rect_rounds_and_clamps_degenerate() {
        // 亚像素 rect 取整后成 1px 窗口（round，非 floor/ceil 截断）。
        let rect = egui::Rect::from_min_max(egui::pos2(0.4, 0.4), egui::pos2(0.6, 0.6));
        assert_eq!(to_px_rect(rect, 1.0), [0, 0, 1, 1]);
        // 高 DPI 放大后同 rect 落在 1px 附近。
        assert_eq!(to_px_rect(rect, 1.25), [1, 1, 1, 1]);
        let rect = egui::Rect::from_min_max(egui::pos2(10.2, 10.6), egui::pos2(20.7, 20.4));
        assert_eq!(to_px_rect(rect, 1.0), [10, 11, 21, 20]);
    }

}