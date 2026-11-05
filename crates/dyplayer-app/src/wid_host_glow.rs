//! glow 后端空实现（老设备版 SW-only）：无 wid GPU 路径。
//! VideoHost::create_hidden() 恒返 None → gpu_wid 恒 false → 走 SW 路径。

use windows::Win32::Foundation::HWND;

pub struct VideoHost;

/// 逻辑坐标 rect → 客户区物理像素 rect（与 wgpu 版同实现，纯数学）。
pub fn to_px_rect(rect: egui::Rect, ppp: f32) -> [i32; 4] {
    let l = (rect.left() * ppp).round() as i32;
    let t = (rect.top() * ppp).round() as i32;
    let r = (rect.right() * ppp).round() as i32;
    let b = (rect.bottom() * ppp).round() as i32;
    [l, t, r.max(l), b.max(t)]
}

impl VideoHost {
    pub fn create_hidden() -> Option<Self> {
        None
    }
    pub fn hwnd(&self) -> isize {
        0
    }
    pub fn attach(&mut self, _parent: HWND) {}
    pub fn align(&mut self, _rect_px: [i32; 4]) {}
    pub fn park_offscreen(&mut self) {}
}