//! 全局明暗主题解析：三态配置（跟随系统/浅色/深色）+ Windows 系统明暗检测。
//!
//! egui/eframe 0.37 没有系统主题跟随能力（不监听 WM_SETTINGCHANGE），
//! 这里直接读注册表 AppsUseLightTheme（Win10 1709+ 的"深色模式"开关），
//! 500ms 缓存避免每帧查询；查询失败（旧系统）按浅色回退。
//!
//! 2026-10-20 U0 全局化：验证页打磨的 Palette（16 语义色 × 深浅各 4 组
//! accent 变体）从早期 library_demo 验证页（现 browse_ui）迁入，成为全应用主题层——自绘 UI 直接
//! 取色，标准件（按钮/TextEdit/Panel/选择态）经 apply_style 映射进
//! egui Style。索引持久化在 config [ui].accent。

use dyplayer_store::ThemeMode;
use egui::{Color32, Stroke};

/// 主题色板（16 语义色）。自绘 UI 的唯一定色来源。
#[derive(Clone, Copy)]
pub struct Palette {
    pub name: &'static str,
    /// 窗口 gutter 底色（两张卡之间露出）。
    pub window: Color32,
    /// 卡片（侧栏 / 主区）底色。
    pub panel: Color32,
    /// 行 / 小卡底色。
    pub surface: Color32,
    pub surface_hover: Color32,
    pub surface_active: Color32,
    pub outline: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
    pub warning: Color32,
}

/// 当前明暗模式下可用的色板组（深浅各 4 套，索引共用）。
pub fn palettes(dark: bool) -> &'static [Palette] {
    if dark {
        &DARK_PALETTES
    } else {
        &LIGHT_PALETTES
    }
}

/// 按索引取色板（越界回退 0）。
pub fn palette(dark: bool, idx: usize) -> Palette {
    let list = palettes(dark);
    list.get(idx).copied().unwrap_or(list[0])
}

/// 深色组（原 4 套主题，window gutter + 浮起卡片）。
static DARK_PALETTES: [Palette; 4] = [
    Palette {
        name: "橙",
        window: Color32::from_rgb(0x10, 0x10, 0x14),
        panel: Color32::from_rgb(0x17, 0x17, 0x1C),
        surface: Color32::from_rgb(0x1E, 0x1E, 0x24),
        surface_hover: Color32::from_rgb(0x26, 0x26, 0x2E),
        surface_active: Color32::from_rgb(0x2E, 0x2E, 0x38),
        outline: Color32::from_rgb(0x2C, 0x2C, 0x34),
        text: Color32::from_rgb(0xF0, 0xF0, 0xF2),
        secondary: Color32::from_rgb(0xA8, 0xA8, 0xB0),
        dim: Color32::from_rgb(0x6E, 0x6E, 0x78),
        accent: Color32::from_rgb(0xFF, 0x82, 0x32),
        accent_hover: Color32::from_rgb(0xFF, 0x9A, 0x55),
        on_accent: Color32::from_rgb(0x17, 0x17, 0x1C),
        danger: Color32::from_rgb(0xE5, 0x53, 0x4B),
        warning: Color32::from_rgb(0xE0, 0xA6, 0x40),
    },
    Palette {
        name: "蓝",
        window: Color32::from_rgb(0x0F, 0x11, 0x15),
        panel: Color32::from_rgb(0x15, 0x18, 0x1E),
        surface: Color32::from_rgb(0x1C, 0x20, 0x27),
        surface_hover: Color32::from_rgb(0x24, 0x29, 0x32),
        surface_active: Color32::from_rgb(0x2C, 0x32, 0x3D),
        outline: Color32::from_rgb(0x2A, 0x30, 0x39),
        text: Color32::from_rgb(0xF0, 0xF2, 0xF4),
        secondary: Color32::from_rgb(0xA6, 0xAE, 0xBA),
        dim: Color32::from_rgb(0x6C, 0x74, 0x80),
        accent: Color32::from_rgb(0x4A, 0x9F, 0xE8),
        accent_hover: Color32::from_rgb(0x6C, 0xB4, 0xEE),
        on_accent: Color32::from_rgb(0x0F, 0x11, 0x15),
        danger: Color32::from_rgb(0xE5, 0x53, 0x4B),
        warning: Color32::from_rgb(0xE0, 0xA6, 0x40),
    },
    Palette {
        name: "绿",
        window: Color32::from_rgb(0x0F, 0x12, 0x10),
        panel: Color32::from_rgb(0x15, 0x19, 0x17),
        surface: Color32::from_rgb(0x1C, 0x21, 0x1E),
        surface_hover: Color32::from_rgb(0x24, 0x2A, 0x26),
        surface_active: Color32::from_rgb(0x2C, 0x33, 0x2E),
        outline: Color32::from_rgb(0x2A, 0x31, 0x2C),
        text: Color32::from_rgb(0xF0, 0xF2, 0xF1),
        secondary: Color32::from_rgb(0xA6, 0xB0, 0xAA),
        dim: Color32::from_rgb(0x6C, 0x76, 0x70),
        accent: Color32::from_rgb(0x43, 0xCB, 0x44),
        accent_hover: Color32::from_rgb(0x66, 0xD9, 0x67),
        on_accent: Color32::from_rgb(0x0F, 0x12, 0x10),
        danger: Color32::from_rgb(0xE5, 0x53, 0x4B),
        warning: Color32::from_rgb(0xE0, 0xA6, 0x40),
    },
    Palette {
        name: "紫",
        window: Color32::from_rgb(0x11, 0x0F, 0x15),
        panel: Color32::from_rgb(0x18, 0x15, 0x1E),
        surface: Color32::from_rgb(0x1F, 0x1C, 0x27),
        surface_hover: Color32::from_rgb(0x28, 0x24, 0x32),
        surface_active: Color32::from_rgb(0x30, 0x2B, 0x3D),
        outline: Color32::from_rgb(0x2E, 0x29, 0x39),
        text: Color32::from_rgb(0xF1, 0xF0, 0xF4),
        secondary: Color32::from_rgb(0xAC, 0xA6, 0xBA),
        dim: Color32::from_rgb(0x72, 0x6C, 0x80),
        accent: Color32::from_rgb(0xB4, 0x8E, 0xF0),
        accent_hover: Color32::from_rgb(0xC6, 0xA7, 0xF4),
        on_accent: Color32::from_rgb(0x11, 0x0F, 0x15),
        danger: Color32::from_rgb(0xE5, 0x53, 0x4B),
        warning: Color32::from_rgb(0xE0, 0xA6, 0x40),
    },
];

/// 浅色组：白卡浮在暖灰 gutter 上，accent 加深保证白底可读（与深色组同索引配对）。
static LIGHT_PALETTES: [Palette; 4] = [
    Palette {
        name: "橙（浅色）",
        window: Color32::from_rgb(0xEF, 0xF0, 0xF3),
        panel: Color32::from_rgb(0xFA, 0xFA, 0xFB),
        surface: Color32::from_rgb(0xF1, 0xF2, 0xF4),
        surface_hover: Color32::from_rgb(0xE9, 0xEA, 0xEE),
        surface_active: Color32::from_rgb(0xE0, 0xE2, 0xE8),
        outline: Color32::from_rgb(0xE3, 0xE4, 0xE9),
        text: Color32::from_rgb(0x1F, 0x20, 0x24),
        secondary: Color32::from_rgb(0x5C, 0x60, 0x68),
        dim: Color32::from_rgb(0x9C, 0xA1, 0xAC),
        accent: Color32::from_rgb(0xE5, 0x6F, 0x1E),
        accent_hover: Color32::from_rgb(0xF2, 0x8A, 0x3F),
        on_accent: Color32::WHITE,
        danger: Color32::from_rgb(0xD9, 0x40, 0x3A),
        warning: Color32::from_rgb(0xC9, 0x86, 0x1B),
    },
    Palette {
        name: "蓝（浅色）",
        window: Color32::from_rgb(0xEE, 0xF0, 0xF3),
        panel: Color32::from_rgb(0xF9, 0xFA, 0xFC),
        surface: Color32::from_rgb(0xF0, 0xF2, 0xF6),
        surface_hover: Color32::from_rgb(0xE7, 0xEA, 0xF0),
        surface_active: Color32::from_rgb(0xDD, 0xE2, 0xEA),
        outline: Color32::from_rgb(0xE0, 0xE4, 0xEB),
        text: Color32::from_rgb(0x1E, 0x21, 0x26),
        secondary: Color32::from_rgb(0x5A, 0x61, 0x70),
        dim: Color32::from_rgb(0x99, 0xA1, 0xB0),
        accent: Color32::from_rgb(0x2E, 0x7F, 0xD6),
        accent_hover: Color32::from_rgb(0x4A, 0x96, 0xE0),
        on_accent: Color32::WHITE,
        danger: Color32::from_rgb(0xD9, 0x40, 0x3A),
        warning: Color32::from_rgb(0xC9, 0x86, 0x1B),
    },
    Palette {
        name: "绿（浅色）",
        window: Color32::from_rgb(0xED, 0xF0, 0xEE),
        panel: Color32::from_rgb(0xF8, 0xFA, 0xF8),
        surface: Color32::from_rgb(0xEF, 0xF2, 0xEF),
        surface_hover: Color32::from_rgb(0xE5, 0xEA, 0xE5),
        surface_active: Color32::from_rgb(0xDA, 0xE1, 0xDA),
        outline: Color32::from_rgb(0xDF, 0xE5, 0xDF),
        text: Color32::from_rgb(0x1E, 0x23, 0x20),
        secondary: Color32::from_rgb(0x5A, 0x66, 0x5D),
        dim: Color32::from_rgb(0x97, 0xA4, 0x9A),
        accent: Color32::from_rgb(0x2E, 0x9E, 0x3C),
        accent_hover: Color32::from_rgb(0x48, 0xB4, 0x56),
        on_accent: Color32::WHITE,
        danger: Color32::from_rgb(0xD9, 0x40, 0x3A),
        warning: Color32::from_rgb(0xC9, 0x86, 0x1B),
    },
    Palette {
        name: "紫（浅色）",
        window: Color32::from_rgb(0xEF, 0xEE, 0xF3),
        panel: Color32::from_rgb(0xF9, 0xF8, 0xFB),
        surface: Color32::from_rgb(0xF1, 0xF0, 0xF5),
        surface_hover: Color32::from_rgb(0xE8, 0xE7, 0xEF),
        surface_active: Color32::from_rgb(0xDE, 0xDA, 0xE8),
        outline: Color32::from_rgb(0xE2, 0xDF, 0xEB),
        text: Color32::from_rgb(0x21, 0x20, 0x26),
        secondary: Color32::from_rgb(0x5F, 0x5A, 0x70),
        dim: Color32::from_rgb(0x9E, 0x99, 0xB0),
        accent: Color32::from_rgb(0x8A, 0x63, 0xD8),
        accent_hover: Color32::from_rgb(0x9E, 0x7E, 0xE4),
        on_accent: Color32::WHITE,
        danger: Color32::from_rgb(0xD9, 0x40, 0x3A),
        warning: Color32::from_rgb(0xC9, 0x86, 0x1B),
    },
];

/// 把 Palette 映射进 egui 全局 Style（标准件观感与验证页自绘对齐）。
/// 必须在 ctx.set_theme() 之后调用——set_theme 会重置为默认 dark/light
/// style，本函数在其上覆盖语义色。播放页的局部深色 override 不受影响
///（Ui::visuals_mut 是本地副本，会整体替换）。
pub fn apply_style(ctx: &egui::Context, dark: bool, accent_idx: usize) {
    let p = palette(dark, accent_idx);
    // egui 0.37：Context 无 style()/set_style()，按当前主题读改写（style_of/set_style_of）
    let mut style = (*ctx.style_of(ctx.theme())).clone();
    let v = &mut style.visuals;
    v.panel_fill = p.window;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.surface;
    v.code_bg_color = p.surface_active;
    v.hyperlink_color = p.accent;
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.outline);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.secondary);
    v.widgets.inactive.bg_fill = p.surface;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.bg_fill = p.surface_hover;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.bg_fill = p.surface_active;
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.open.bg_fill = p.surface_active;
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, p.on_accent);
    ctx.set_style_of(ctx.theme(), style);
}

/// 主题模式 → 实际是否深色（System 时查询系统偏好）。
pub fn effective_dark(mode: ThemeMode) -> bool {
    match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => !system_prefers_light(),
    }
}

thread_local! {
    static SYS_LIGHT_CACHE: std::cell::RefCell<Option<(std::time::Instant, bool)>> =
        const { std::cell::RefCell::new(None) };
}

/// 系统当前是否偏好浅色应用主题（带 500ms 缓存）。
pub fn system_prefers_light() -> bool {
    let now = std::time::Instant::now();
    SYS_LIGHT_CACHE.with(|c| {
        if let Some((at, v)) = *c.borrow()
            && now.duration_since(at) < std::time::Duration::from_millis(500)
        {
            return v;
        }
        let v = read_registry().unwrap_or(true);
        *c.borrow_mut() = Some((now, v));
        v
    })
}

/// 读 HKCU\...\Themes\Personalize 的 AppsUseLightTheme（1=浅色 0=深色）。
#[cfg(windows)]
fn read_registry() -> Option<bool> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::core::w;
    unsafe {
        let mut hkey = HKEY::default();
        let opened = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            None,
            KEY_READ,
            &mut hkey,
        );
        if opened.is_err() {
            return None;
        }
        let mut ty = REG_VALUE_TYPE::default();
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let queried = RegQueryValueExW(
            hkey,
            w!("AppsUseLightTheme"),
            None,
            Some(&mut ty),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if queried.is_ok() {
            Some(data != 0)
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
fn read_registry() -> Option<bool> {
    None
}

/// 启动预涂窗口底色：eframe 建窗后、首帧 present 前客户区是 Win32
/// 窗口类默认白底；深色主题下 new() 的耗时初始化（SQLite + 字体解析）
/// 期间表现为"标题先浅色后深色"的启动白闪。在 app creator 里用 GDI
/// 把客户区涂成首帧底色（由调用方按启动视图决定），白闪窗口期从
/// new() 全程缩短到建窗→涂色的毫秒级；首帧 present 后自然接管。
#[cfg(windows)]
pub fn prepaint_window(title: &str, fill: egui::Color32) {
    use windows::Win32::Foundation::{COLORREF, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateSolidBrush, DeleteObject, FillRect, GetDC, HGDIOBJ, ReleaseDC,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetClientRect, GetWindowThreadProcessId,
    };
    use windows::core::PCWSTR;
    let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(hwnd) = FindWindowW(None, PCWSTR::from_raw(title_w.as_ptr())) else {
            return;
        };
        // 同名窗口防御：只涂本进程的窗口。
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != std::process::id() {
            return;
        }
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_err() {
            return;
        }
        let hdc = GetDC(Some(hwnd));
        // COLORREF 布局 0x00BBGGRR。
        let color = COLORREF(fill.r() as u32 | (fill.g() as u32) << 8 | (fill.b() as u32) << 16);
        let brush = CreateSolidBrush(color);
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        ReleaseDC(Some(hwnd), hdc);
    }
}

#[cfg(not(windows))]
pub fn prepaint_window(_title: &str, _fill: egui::Color32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_modes_map_directly() {
        assert!(effective_dark(ThemeMode::Dark));
        assert!(!effective_dark(ThemeMode::Light));
    }

    #[test]
    fn system_query_does_not_panic() {
        // 不断言具体值（取决于机器设置），只保证可查询且稳定返回。
        let a = system_prefers_light();
        let b = system_prefers_light();
        assert_eq!(a, b);
    }

    #[test]
    fn palette_index_out_of_range_falls_back_to_first() {
        assert_eq!(palette(true, 99).name, DARK_PALETTES[0].name);
        assert_eq!(palette(false, 99).name, LIGHT_PALETTES[0].name);
        assert_eq!(palette(true, 2).name, DARK_PALETTES[2].name);
    }

    #[test]
    fn palettes_have_matching_variants() {
        // 深浅同索引配对（名字前缀对应），共 4 组。
        assert_eq!(DARK_PALETTES.len(), 4);
        assert_eq!(LIGHT_PALETTES.len(), 4);
        for i in 0..4 {
            assert!(DARK_PALETTES[i].name.starts_with(LIGHT_PALETTES[i].name.split('（').next().unwrap()));
        }
    }

    #[test]
    fn apply_style_overrides_visuals() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Light);
        apply_style(&ctx, false, 0);
        let p = palette(false, 0);
        assert_eq!(ctx.style_of(ctx.theme()).visuals.panel_fill, p.window);
        assert_eq!(
            ctx.style_of(ctx.theme()).visuals.selection.bg_fill,
            p.accent
        );
        // 播放页局部 override 不受全局 style 影响：root Ui visuals 可独立替换。
        apply_style(&ctx, false, 1);
        assert_eq!(
            ctx.style_of(ctx.theme()).visuals.panel_fill,
            palette(false, 1).window
        );
    }
}
