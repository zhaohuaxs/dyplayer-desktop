//! Windows 文件右键菜单"使用 DyPlayer 打开"注册/注销。
//!
//! 两套独立注册：
//! 1. 右键菜单项（HKCU\Software\Classes\*\shell\DyPlayer + AppliesTo 限定视频扩展名）
//! 2. "打开方式"列表（ProgID DyPlayer.Video + 各扩展名 OpenWithProgids）
//!
//! 用 HKCU（当前用户），不需要管理员权限。每次注册用 current_exe()
//! 动态取路径，便携模式换位置后启动 sync() 自动修正。

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};

const SHELL_KEY: &str = "Software\\Classes\\*\\shell\\DyPlayer";
const COMMAND_KEY: &str = "Software\\Classes\\*\\shell\\DyPlayer\\command";
const MENU_LABEL: &str = "使用 DyPlayer 打开";

const PROG_ID: &str = "Software\\Classes\\DyPlayer.Video";
const PROG_ID_COMMAND: &str = "Software\\Classes\\DyPlayer.Video\\shell\\open\\command";
const PROG_ID_ICON: &str = "Software\\Classes\\DyPlayer.Video\\DefaultIcon";

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 构建 AppliesTo AQS 条件字符串，限定右键菜单仅对视频扩展名显示。
/// 格式：`System.FileExtension:=".mp4" OR System.FileExtension:=".mkv" OR ...`
fn build_applies_to() -> String {
    dyplayer_vfs::VIDEO_EXTENSIONS
        .iter()
        .map(|ext| format!("System.FileExtension:=\".{ext}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// 注册右键菜单项：创建 shell 键（菜单文字）+ command 子键（执行命令）。
pub fn register() -> anyhow::Result<()> {
    let exe_path = std::env::current_exe()?;
    let exe_str = exe_path.to_string_lossy();
    let cmd = format!("\"{exe_str}\" \"%1\"");

    unsafe {
        let shell_key = to_wide(SHELL_KEY);
        let mut hkey = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(shell_key.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if status.is_err() {
            anyhow::bail!("RegCreateKeyExW(shell) failed: {:?}", status);
        }

        let label = to_wide(MENU_LABEL);
        let status = windows::Win32::System::Registry::RegSetValueExW(
            hkey,
            PCWSTR::null(),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                label.as_ptr() as *const u8,
                label.len() * 2,
            )),
        );
        if status.is_err() {
            let _ = RegCloseKey(hkey);
            anyhow::bail!("RegSetValueExW(label) failed: {:?}", status);
        }

        let applies_to = build_applies_to();
        let applies_wide = to_wide(&applies_to);
        let applies_name = to_wide("AppliesTo");
        let status = windows::Win32::System::Registry::RegSetValueExW(
            hkey,
            PCWSTR(applies_name.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                applies_wide.as_ptr() as *const u8,
                applies_wide.len() * 2,
            )),
        );
        let _ = RegCloseKey(hkey);
        if status.is_err() {
            anyhow::bail!("RegSetValueExW(AppliesTo) failed: {:?}", status);
        }

        let cmd_key = to_wide(COMMAND_KEY);
        let mut hcmd = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(cmd_key.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hcmd,
            None,
        );
        if status.is_err() {
            anyhow::bail!("RegCreateKeyExW(command) failed: {:?}", status);
        }

        let cmd_wide = to_wide(&cmd);
        let status = windows::Win32::System::Registry::RegSetValueExW(
            hcmd,
            PCWSTR::null(),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                cmd_wide.as_ptr() as *const u8,
                cmd_wide.len() * 2,
            )),
        );
        let _ = RegCloseKey(hcmd);
        if status.is_err() {
            anyhow::bail!("RegSetValueExW(command) failed: {:?}", status);
        }
    }

    tracing::info!("右键菜单已注册: {exe_str}");
    Ok(())
}

/// 注销右键菜单项：删除 command 子键后删除 shell 键。
pub fn unregister() -> anyhow::Result<()> {
    unsafe {
        let cmd_key = to_wide(COMMAND_KEY);
        let status = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(cmd_key.as_ptr()));
        if status.is_err() {
            tracing::warn!("RegDeleteKeyW(command) failed: {:?}", status);
        }

        let shell_key = to_wide(SHELL_KEY);
        let status = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(shell_key.as_ptr()));
        if status.is_err() {
            anyhow::bail!("RegDeleteKeyW(shell) failed: {:?}", status);
        }
    }

    tracing::info!("右键菜单已注销");
    Ok(())
}

/// 检查右键菜单是否已注册（shell 键存在且 command 值匹配当前 exe 路径）。
pub fn is_registered() -> bool {
    unsafe {
        let shell_key = to_wide(SHELL_KEY);
        let mut hkey = HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(shell_key.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        );
        if status.is_err() {
            return false;
        }
        let _ = RegCloseKey(hkey);

        let cmd_key = to_wide(COMMAND_KEY);
        let mut hcmd = HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(cmd_key.as_ptr()),
            None,
            KEY_READ,
            &mut hcmd,
        );
        if status.is_err() {
            return false;
        }

        let mut buf = [0u16; 1024];
        let mut len: u32 = buf.len() as u32 * 2;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(cmd_key.as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        );
        let _ = RegCloseKey(hcmd);
        if status.is_err() {
            return false;
        }

        let str_len = (len as usize) / 2;
        let registered_cmd: String = buf[..str_len]
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
            .collect();

        let exe_path = match std::env::current_exe() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => return true,
        };
        let expected_cmd = format!("\"{exe_path}\" \"%1\"");
        registered_cmd == expected_cmd
    }
}

/// 按配置同步注册表状态：
/// - enabled=true 但未注册 → 注册
/// - enabled=false 但已注册 → 注销
/// - enabled=true 且已注册但 exe 路径变化 → 重新注册
pub fn sync(enabled: bool) {
    let registered = is_registered();
    if enabled && !registered {
        if let Err(e) = register() {
            tracing::warn!("右键菜单注册失败: {e}");
        }
    } else if !enabled && registered {
        if let Err(e) = unregister() {
            tracing::warn!("右键菜单注销失败: {e}");
        }
    } else if enabled && registered {
        tracing::debug!("右键菜单已注册且路径匹配，无需操作");
    }
}
// ── "打开方式"注册（ProgID + OpenWithProgids） ──

/// 创建一个注册表键并返回句柄。
unsafe fn create_key(path: &str) -> anyhow::Result<HKEY> {
    unsafe {
        let wide = to_wide(path);
        let mut hkey = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if status.is_err() {
            anyhow::bail!("RegCreateKeyExW({path}) failed: {:?}", status);
        }
        Ok(hkey)
    }
}

/// 设置 REG_SZ 值（value_name 为空串时写默认值）。
unsafe fn set_sz(hkey: HKEY, value_name: &str, data: &str) -> anyhow::Result<()> {
    unsafe {
        let name = to_wide(value_name);
        let wide = to_wide(data);
        let status = windows::Win32::System::Registry::RegSetValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * 2,
            )),
        );
        if status.is_err() {
            anyhow::bail!("RegSetValueExW({value_name}) failed: {:?}", status);
        }
        Ok(())
    }
}

/// 读取 REG_SZ 默认值。
unsafe fn get_default_sz(path: &str) -> Option<String> {
    unsafe {
        let wide = to_wide(path);
        let mut buf = [0u16; 1024];
        let mut len: u32 = buf.len() as u32 * 2;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        );
        if status.is_err() {
            return None;
        }
        let str_len = (len as usize) / 2;
        Some(
            buf[..str_len]
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
                .collect(),
        )
    }
}

/// 注册"打开方式"：创建 ProgID + 为每个视频扩展名注册 OpenWithProgids。
pub fn register_open_with() -> anyhow::Result<()> {
    let exe_path = std::env::current_exe()?;
    let exe_str = exe_path.to_string_lossy();
    let cmd = format!("\"{exe_str}\" \"%1\"");
    let icon = format!("\"{exe_str}\",0");

    unsafe {
        // 1. ProgID 根键 + FriendlyTypeName（"打开方式"列表显示名）
        let hkey = create_key(PROG_ID)?;
        set_sz(hkey, "FriendlyTypeName", "DyPlayer")?;
        let _ = RegCloseKey(hkey);

        // 2. ProgID\shell\open\command
        let hcmd = create_key(PROG_ID_COMMAND)?;
        set_sz(hcmd, "", &cmd)?;
        let _ = RegCloseKey(hcmd);

        // 3. ProgID\DefaultIcon
        let hicon = create_key(PROG_ID_ICON)?;
        set_sz(hicon, "", &icon)?;
        let _ = RegCloseKey(hicon);

        // 4. 每个扩展名 → OpenWithProgids\DyPlayer.Video
        for ext in dyplayer_vfs::VIDEO_EXTENSIONS {
            let ext_key = format!("Software\\Classes\\.{ext}\\OpenWithProgids");
            let hext = create_key(&ext_key)?;
            set_sz(hext, "DyPlayer.Video", "")?;
            let _ = RegCloseKey(hext);
        }
    }

    tracing::info!("打开方式已注册: {exe_str}");
    Ok(())
}

/// 注销"打开方式"：删除各扩展名的 OpenWithProgids 值 + 删除 ProgID。
pub fn unregister_open_with() -> anyhow::Result<()> {
    unsafe {
        // 1. 删除每个扩展名的 OpenWithProgids\DyPlayer.Video 值
        for ext in dyplayer_vfs::VIDEO_EXTENSIONS {
            let ext_key = format!("Software\\Classes\\.{ext}\\OpenWithProgids");
            let wide_path = to_wide(&ext_key);
            let mut hext = HKEY::default();
            let status = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_path.as_ptr()),
                None,
                KEY_WRITE,
                &mut hext,
            );
            if status.is_err() {
                continue;
            }
            let wide_name = to_wide("DyPlayer.Video");
            let status = RegDeleteValueW(hext, PCWSTR(wide_name.as_ptr()));
            let _ = RegCloseKey(hext);
            if status.is_err() {
                tracing::debug!("RegDeleteValueW(.{ext}\\OpenWithProgids\\DyPlayer.Video) failed: {:?}", status);
            }
        }

        // 2. 删除 ProgID 子键（command、DefaultIcon、shell、shell\open）
        for sub in [
            "Software\\Classes\\DyPlayer.Video\\shell\\open\\command",
            "Software\\Classes\\DyPlayer.Video\\shell\\open",
            "Software\\Classes\\DyPlayer.Video\\shell",
            "Software\\Classes\\DyPlayer.Video\\DefaultIcon",
        ] {
            let wide = to_wide(sub);
            let status = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()));
            if status.is_err() {
                tracing::debug!("RegDeleteKeyW({sub}) failed: {:?}", status);
            }
        }

        // 3. 删除 ProgID 根键
        let wide = to_wide(PROG_ID);
        let status = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()));
        if status.is_err() {
            anyhow::bail!("RegDeleteKeyW(ProgID) failed: {:?}", status);
        }
    }

    tracing::info!("打开方式已注销");
    Ok(())
}

/// 检查"打开方式"是否已注册（ProgID command 值匹配当前 exe 路径）。
pub fn is_open_with_registered() -> bool {
    unsafe {
        let cmd = match get_default_sz(PROG_ID_COMMAND) {
            Some(c) => c,
            None => return false,
        };

        let exe_path = match std::env::current_exe() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => return true,
        };
        let expected_cmd = format!("\"{exe_path}\" \"%1\"");
        cmd == expected_cmd
    }
}

/// 按配置同步"打开方式"注册表状态。
pub fn sync_open_with(enabled: bool) {
    let registered = is_open_with_registered();
    if enabled && !registered {
        if let Err(e) = register_open_with() {
            tracing::warn!("打开方式注册失败: {e}");
        }
    } else if !enabled && registered {
        if let Err(e) = unregister_open_with() {
            tracing::warn!("打开方式注销失败: {e}");
        }
    } else if enabled && registered {
        tracing::debug!("打开方式已注册且路径匹配，无需操作");
    }
}