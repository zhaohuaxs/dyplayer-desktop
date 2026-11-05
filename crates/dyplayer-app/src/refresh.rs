//! 窗口所在显示器精确刷新率检测。
//!
//! mpv display 配速需要 display-fps-override 与实际信号频率一致
//! （60 屏写 60 没问题，但 59.94/119.88 屏写整数会造成时钟微漂）。
//! 检测优先级：QueryDisplayConfig 的 targetVideoSignalInfo.vSyncFreq
//! （实际视频信号，分数值）→ 路径 refreshRate → EnumDisplaySettings
//! 的 dmDisplayFrequency（59/119 等 NTSC 值换算 (n+1)/1.001）。
//! 结果只在首个 eframe 帧检测一次并缓存。

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const MIN_HZ: f64 = 20.0;
const MAX_HZ: f64 = 1000.0;

fn plausible(hz: f64) -> Option<f64> {
    (hz.is_finite() && (MIN_HZ..=MAX_HZ).contains(&hz)).then_some(hz)
}

/// DISPLAYCONFIG_RATIONAL 分数刷新率（如 60000/1001 = 59.94）。
fn rational_hz(num: u32, den: u32) -> Option<f64> {
    (num > 0 && den > 0).then(|| plausible(f64::from(num) / f64::from(den)))?
}

/// EnumDisplaySettings 整数刷新率：0/1 = 硬件预设；59/119 等实为 (n+1)/1.001。
fn devmode_hz(n: u32) -> Option<f64> {
    match n {
        0 | 1 => None,
        23 | 29 | 47 | 59 | 71 | 119 | 143 | 239 => plausible(f64::from(n + 1) / 1.001),
        _ => plausible(f64::from(n)),
    }
}

/// 同一桌面克隆到多个显示器（clone 模式）时各屏查到的刷新率。
enum Clones {
    /// 一致（或单屏）
    Rate(f64),
    /// 一个都没查到：走兜底手段
    Nothing,
    /// 不一致：present 跟哪个屏同步未知，视为查不到
    Disagree,
}

fn clone_rate(rates: &[f64]) -> Clones {
    match rates.split_first() {
        None => Clones::Nothing,
        Some((first, rest)) if rest.iter().all(|r| (r - first).abs() < 0.005) => Clones::Rate(*first),
        Some(_) => Clones::Disagree,
    }
}

static REFRESH_HZ: LazyLock<Mutex<Option<f64>>> = LazyLock::new(|| Mutex::new(None));
static LAST_DETECT: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));
/// 重检测节流间隔（R09：跨屏/显示模式切换后缓存会过期，定期重检）。
const REDETECT_INTERVAL: Duration = Duration::from_secs(5);

/// eframe 帧回调里调用；定期重检测（5s 节流）并更新缓存（R09）。
/// PlayerSession::new 读 [`cached_hz`] 注入 mpv display 配速；重检测后
/// 新会话用最新值，现有会话不动态更新（留完整修复）。
pub fn prime(frame: &eframe::Frame) {
    let now = Instant::now();
    {
        let last = LAST_DETECT.lock().unwrap();
        if let Some(t) = *last
            && now.duration_since(t) < REDETECT_INTERVAL
        {
            return;
        }
    }
    let hz = detect(frame);
    match hz {
        Some(hz) => tracing::info!("屏幕刷新率检测: {hz:.3} Hz"),
        None => tracing::warn!("屏幕刷新率检测失败，mpv 保持视频帧率配速"),
    }
    *REFRESH_HZ.lock().unwrap() = hz;
    *LAST_DETECT.lock().unwrap() = Some(now);
}

/// 已缓存的窗口所在屏刷新率（Hz）；None = 未检测/检测失败/非 Windows。
/// None 时调用方应退回 mpv 默认（audio 时钟）配速。
pub fn cached_hz() -> Option<f64> {
    *REFRESH_HZ.lock().unwrap()
}

fn detect(frame: &eframe::Frame) -> Option<f64> {
    use raw_window_handle::HasWindowHandle;
    let handle = frame.window_handle().ok()?;
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    window_refresh_hz(win32.hwnd.get())
}

#[cfg(windows)]
fn window_refresh_hz(hwnd: isize) -> Option<f64> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
        QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND};
    use windows::Win32::Graphics::Gdi::{
        ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HMONITOR, MonitorFromWindow,
        MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    };

    fn same_name(a: &[u16; 32], b: &[u16; 32]) -> bool {
        let trim = |s: &[u16]| s.iter().position(|c| *c == 0).unwrap_or(s.len());
        a[..trim(a)] == b[..trim(b)]
    }

    /// 当前使用的输出路径与模式。两次查询之间显示设置变化会返回
    /// ERROR_INSUFFICIENT_BUFFER，重查（最多 3 次）。
    fn active_paths()
    -> Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
        for _ in 0..3 {
            let (mut np, mut nm) = (0u32, 0u32);
            // SAFETY: 单纯系统调用，出参长度即缓冲区容量
            if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }
                != ERROR_SUCCESS
            {
                return None;
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            // SAFETY: 数组长度与传入数量一致；topology 传 null（无 QDC_DATABASE_CURRENT）
            let r = unsafe {
                QueryDisplayConfig(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut np,
                    paths.as_mut_ptr(),
                    &mut nm,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            match r {
                ERROR_SUCCESS => {
                    paths.truncate(np as usize);
                    modes.truncate(nm as usize);
                    return Some((paths, modes));
                }
                ERROR_INSUFFICIENT_BUFFER => continue,
                _ => return None,
            }
        }
        None
    }

    // SAFETY: 纯系统调用；窗口不存在时回最近屏
    let hmon: HMONITOR = unsafe { MonitorFromWindow(HWND(hwnd as *mut _), MONITOR_DEFAULTTONEAREST) };
    if hmon.is_invalid() {
        return None;
    }

    // GDI 设备名（\\.\DISPLAY1）：与 QueryDisplayConfig 的 source 匹配键
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: MONITORINFOEXW 开头就是 MONITORINFO，cbSize 告知实际大小
    if !unsafe { GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
        .as_bool()
    {
        return None;
    }
    let device = info.szDevice;

    // 实际视频信号 vSyncFreq 优先；没有才用路径上设定的 refreshRate
    let mut rates: Vec<f64> = Vec::new();
    if let Some((paths, modes)) = active_paths() {
        for path in &paths {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            // SAFETY: header 的 type/size 与实际结构一致
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0
                || !same_name(&source.viewGdiDeviceName, &device)
            {
                continue;
            }
            // SAFETY: 读 union 的 u32 字段，任意位组合都合法
            let idx = unsafe { path.targetInfo.Anonymous.modeInfoIdx } as usize;
            let vsync = modes
                .get(idx)
                .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET)
                .and_then(|m| {
                    // SAFETY: infoType 是 TARGET，union 里是 targetMode
                    let f = unsafe { m.Anonymous.targetMode.targetVideoSignalInfo.vSyncFreq };
                    rational_hz(f.Numerator, f.Denominator)
                });
            let r = path.targetInfo.refreshRate;
            if let Some(hz) = vsync.or_else(|| rational_hz(r.Numerator, r.Denominator)) {
                rates.push(hz);
            }
        }
    }
    match clone_rate(&rates) {
        Clones::Rate(hz) => Some(hz),
        Clones::Disagree => None,
        Clones::Nothing => {
            // EnumDisplaySettings 兜底（整数值，NTSC 换算）
            let mut dm = windows::Win32::Graphics::Gdi::DEVMODEW {
                dmSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: device 以 0 结尾；dmSize 已设
            let ok = unsafe {
                EnumDisplaySettingsW(
                    windows::core::PCWSTR(device.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut dm,
                )
            }
            .as_bool();
            ok.then_some(()).and_then(|_| devmode_hz(dm.dmDisplayFrequency))
        }
    }
}

#[cfg(not(windows))]
fn window_refresh_hz(_hwnd: isize) -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_hz_basic() {
        assert_eq!(rational_hz(60000, 1001).map(|h| (h * 1000.0).round() / 1000.0), Some(59.940));
        assert_eq!(rational_hz(60, 1), Some(60.0));
        assert_eq!(rational_hz(0, 1), None);
        assert_eq!(rational_hz(60, 0), None);

    }

    #[test]
    fn devmode_hz_ntsc() {
        assert_eq!(devmode_hz(59).map(|h| (h * 1000.0).round() / 1000.0), Some(59.940));
        assert_eq!(devmode_hz(119).map(|h| (h * 1000.0).round() / 1000.0), Some(119.880));
        assert_eq!(devmode_hz(60), Some(60.0));
        assert_eq!(devmode_hz(144), Some(144.0));
        assert_eq!(devmode_hz(0), None);
        assert_eq!(devmode_hz(1), None);
    }

    #[test]
    fn clone_rate_variants() {
        assert!(matches!(clone_rate(&[]), Clones::Nothing));
        assert!(matches!(clone_rate(&[60.0]), Clones::Rate(h) if h == 60.0));
        assert!(matches!(clone_rate(&[60.0, 60.001]), Clones::Rate(_)));
        assert!(matches!(clone_rate(&[60.0, 120.0]), Clones::Disagree));
    }
}