//! SMB 源（M3，Windows 优先）：UNC 路径 + Windows 内置 SMB 客户端。
//!
//! 路径模型：`smb://host/share[/sub]` → 根 `\\host\share[\sub]`；VFS path 为
//! UNC 全路径（与 LocalFs 的绝对路径语义一致，面包屑/上级导航通用）。
//! 凭证：首次访问前 `WNetAddConnection2W` 建立 `\\host\share` 会话；
//! 网络类错误触发 `WNetCancelConnection2` + 重连一次重试。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::source::VfsSource;
use crate::types::{RangeRead, VfsEntry, VfsError, VfsMeta, VfsResult};

#[derive(Debug, Clone, Default)]
pub struct SmbConfig {
    /// `smb://host/share[/sub]` 或 `\\host\share[\sub]`。
    pub endpoint: String,
    pub username: String,
    pub password: String,
    /// Windows 域（可空；用户名形如 `DOMAIN\user` 时也可不填）。
    pub domain: String,
}

pub struct SmbFs {
    root: PathBuf,
    connect_target: String,
    username: String,
    password: String,
    connected: Mutex<bool>,
}

impl SmbFs {
    pub fn new(config: SmbConfig) -> VfsResult<Self> {
        let (root, connect_target, _host) = parse_smb_endpoint(&config.endpoint)
            .map_err(VfsError::Unsupported)?;
        Ok(Self {
            root,
            connect_target,
            username: join_domain_user(&config.domain, &config.username),
            password: config.password,
            connected: Mutex::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把传入路径规范为本源内的 UNC 路径（`/` → `\`，相对 root 拼接），
    /// 随后做词法规范化（`..` 回退、`.` 透传）——UNC 无法 canonicalize
    /// （LocalFs 靠 canonicalize 等价处理 `..`），字符串前缀判定必须先
    /// 归一，否则 `\\host\share\..\other\x` 可绕过根边界检查。
    fn resolve(&self, path: &Path) -> VfsResult<PathBuf> {
        let normalized = normalize_unc(path);
        let candidate = if path_is_absolute_unc(&normalized) || path_has_prefix(&normalized) {
            normalized
        } else {
            join_unc(&self.root, &normalized)
        };
        let candidate = lexically_normalize(&candidate).ok_or_else(|| VfsError::OutsideRoot {
            root: self.root.display().to_string(),
            path: path.display().to_string(),
        })?;
        if candidate
            .to_string_lossy()
            .eq_ignore_ascii_case(self.root.to_string_lossy().as_ref())
            || starts_with_component(&candidate, &self.root)
        {
            Ok(candidate)
        } else {
            Err(VfsError::OutsideRoot {
                root: self.root.display().to_string(),
                path: candidate.display().to_string(),
            })
        }
    }

    fn ensure_connected(&self) -> VfsResult<()> {
        let mut guard = self
            .connected
            .lock()
            .map_err(|_| VfsError::Network("连接状态锁中毒".into()))?;
        if *guard {
            return Ok(());
        }
        wnet_connect(&self.connect_target, &self.username, &self.password)?;
        *guard = true;
        Ok(())
    }

    fn force_reconnect(&self) {
        if let Ok(mut guard) = self.connected.lock() {
            wnet_disconnect(&self.connect_target);
            *guard = false;
        }
    }

    /// 网络类 IO 错误时重连一次并重试操作（`op` 每次调用都重新发起）。
    fn with_reconnect<T>(&self, mut op: impl FnMut() -> std::io::Result<T>) -> VfsResult<T> {
        self.ensure_connected()?;
        match op() {
            Ok(v) => Ok(v),
            Err(first) if is_network_error(&first) => {
                tracing::warn!("SMB 网络错误（{first}），重连后重试");
                self.force_reconnect();
                self.ensure_connected()?;
                op().map_err(VfsError::from)
            }
            Err(e) => Err(VfsError::from(e)),
        }
    }

    fn list_dir_inner(&self, dir: &Path) -> VfsResult<Vec<VfsEntry>> {
        let mut entries = Vec::new();
        let iter = self.with_reconnect(|| std::fs::read_dir(dir))?;
        for item in iter {
            let item = item.map_err(VfsError::from)?;
            let Ok(file_type) = item.file_type() else {
                tracing::warn!(target: "vfs", "smb list_dir: file_type 失败，跳过条目");
                continue;
            };
            let Ok(name) = item.file_name().into_string() else {
                tracing::warn!(target: "vfs", "smb list_dir: 文件名非 UTF-8，跳过条目");
                continue;
            };
            let is_dir = file_type.is_dir();
            let (size, modified) = match item.metadata() {
                Ok(meta) => (meta.len(), meta.modified().ok()),
                Err(e) => {
                    tracing::warn!(target: "vfs", "smb list_dir: metadata 失败（显示为 0 字节）: {e}");
                    (0, None)
                }
            };
            entries.push(VfsEntry {
                name,
                path: item.path(),
                is_dir,
                size,
                modified,
            });
        }
        Ok(entries)
    }
}

impl VfsSource for SmbFs {
    fn list_dir(&self, path: &Path) -> VfsResult<Vec<VfsEntry>> {
        let dir = self.resolve(path)?;
        self.list_dir_inner(&dir)
    }

    fn stat(&self, path: &Path) -> VfsResult<VfsMeta> {
        let real = self.resolve(path)?;
        let meta = self.with_reconnect(|| std::fs::metadata(&real))?;
        Ok(VfsMeta {
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn read_full(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let real = self.resolve(path)?;
        self.with_reconnect(|| std::fs::read(&real))
            .map_err(|e| match e {
                VfsError::Io(io) => map_io_error(io, &real),
                other => other,
            })
    }

    fn open_range(&self, path: &Path, start: u64, len: Option<u64>) -> VfsResult<RangeRead> {
        use std::io::{Read, Seek, SeekFrom};
        let real = self.resolve(path)?;
        let mut file = self
            .with_reconnect(|| std::fs::File::open(&real))
            .map_err(|e| match e {
                VfsError::Io(io) => map_io_error(io, &real),
                other => other,
            })?;
        let total = file.metadata()?.len();
        if start >= total && total > 0 {
            return Err(VfsError::InvalidRange { start, total });
        }
        file.seek(SeekFrom::Start(start))?;
        let available = total - start;
        let length = len.map_or(available, |l| l.min(available));
        let reader: Box<dyn Read + Send> = match len {
            Some(l) => Box::new(file.take(l)),
            None => Box::new(file),
        };
        Ok(RangeRead { reader, length })
    }
}

fn map_io_error(e: std::io::Error, path: &Path) -> VfsError {
    match e.raw_os_error() {
        Some(5) => VfsError::AccessDenied(format!(
            "访问被拒绝（凭据或共享权限）: {}",
            path.display()
        )),
        Some(1326) => VfsError::AccessDenied("登录失败：用户名或密码错误".into()),
        Some(53) | Some(67) | Some(1231) => {
            VfsError::NotFound(format!("网络路径不可达: {}", path.display()))
        }
        _ => VfsError::Io(e),
    }
}

fn is_network_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(53) | Some(64) | Some(121) | Some(1231) | Some(1311))
}

// ---------- 纯路径函数（单测覆盖，跨平台可测） ----------

/// `\\host\share[\sub]` / `smb://host/share[/sub]` → (root UNC, 连接目标, host)。
pub(crate) fn parse_smb_endpoint(input: &str) -> Result<(PathBuf, String, String), String> {
    let s = input.trim();
    let s = s.strip_suffix('/').unwrap_or(s);
    let s = s.strip_suffix('\\').unwrap_or(s);
    if s.is_empty() {
        return Err("SMB 地址为空".into());
    }
    let unc = if let Some(rest) = s.strip_prefix(r"\\") {
        rest.to_string()
    } else if let Some(rest) = s
        .strip_prefix("smb://")
        .or_else(|| s.strip_prefix("SMB://"))
    {
        rest.replace('/', "\\")
    } else {
        return Err(format!("无法识别的 SMB 地址（应为 \\\\host\\share 或 smb://host/share）: {input}"));
    };
    let mut parts = unc.split('\\').filter(|p| !p.is_empty());
    let host = parts.next().ok_or("缺少主机名")?.to_string();
    if host.is_empty() {
        return Err("缺少主机名".into());
    }
    let share = parts.next().ok_or("缺少共享名")?;
    let sub: Vec<&str> = parts.collect();
    let connect = format!(r"\\{host}\{share}");
    let mut root = PathBuf::from(format!(r"\\{host}\{share}"));
    for p in sub {
        root.push(p);
    }
    Ok((root, connect, host))
}

fn normalize_unc(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().replace('/', r"\");
    PathBuf::from(s)
}

/// UNC 词法规范化（不触文件系统）：`..` 弹段、`.`/空段跳过。
/// host 与 share 是不可弹穿的栈底——`..` 试图逃出 `\\host\share` 之外
/// 时返回 None（调用方按越界处理）；弹掉 share（如 `\\host\share\..`
/// 规范化为 `\\host`）仍返回路径，由后续根边界检查拦截。
/// `\\?\UNC\` 前缀形态剥掉处理后装回。
fn lexically_normalize(unc: &Path) -> Option<PathBuf> {
    let s = unc.to_string_lossy();
    // 快速路径：无 `/`、无 `..`、无独立 `.` 段时零分配返回（resolve 是
    // list_dir/stat/open_range 的热路径，绝大多数路径走此分支）。
    if !s.contains('/')
        && !s.contains("..")
        && !s.contains(r"\.\")
        && !s.ends_with(r"\.")
    {
        return Some(unc.to_path_buf());
    }
    let s = s.replace('/', r"\");
    let (prefix, rest) = if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        (r"\\?\UNC\", r.to_string())
    } else if let Some(r) = s.strip_prefix(r"\\") {
        (r"\\", r.to_string())
    } else {
        // 非 UNC 绝对形态（相对路径已由 join_unc 拼接成 UNC 后进入）；
        // 防御性原样返回。
        return Some(PathBuf::from(s));
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in rest.split('\\') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.len() < 2 {
                    return None;
                }
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(PathBuf::from(format!(r"{prefix}{}", parts.join(r"\"))))
}

fn path_is_absolute_unc(p: &Path) -> bool {
    p.to_string_lossy().starts_with(r"\\")
}

fn path_has_prefix(p: &Path) -> bool {
    p.to_string_lossy().starts_with(r"\\?\UNC\")
}

fn join_unc(base: &Path, rel: &Path) -> PathBuf {
    let mut out = base.to_path_buf();
    for c in rel.components() {
        match c {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn starts_with_component(path: &Path, base: &Path) -> bool {
    let ps = path.to_string_lossy();
    let bs = base.to_string_lossy();
    if ps.len() < bs.len() {
        return false;
    }
    // UNC/Windows 路径大小写不敏感：ASCII 折叠后比较（SMB 服务器对
    // 大小写变体等同处理，客户端根边界判定必须同样宽容，否则
    // `\\NAS\MEDIA\...` 会被误判 OutsideRoot）。
    if !ps[..bs.len()].eq_ignore_ascii_case(bs.as_ref()) {
        return false;
    }
    let rest = &ps[bs.len()..];
    rest.is_empty() || rest.starts_with('\\')
}

fn join_domain_user(domain: &str, username: &str) -> String {
    if domain.is_empty() || username.contains('\\') {
        username.to_string()
    } else {
        format!(r"{domain}\{username}")
    }
}

// ---------- WNet FFI（仅 Windows） ----------

#[cfg(windows)]
fn wnet_connect(target: &str, username: &str, password: &str) -> VfsResult<()> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_ASSIGNED, ERROR_LOGON_FAILURE, ERROR_SESSION_CREDENTIAL_CONFLICT, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::WNet::{NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W};
    use windows::Win32::NetworkManagement::WNet::NET_CONNECT_FLAGS;

    let remote = HSTRING::from(target);
    let user = HSTRING::from(username);
    let pass = HSTRING::from(password);
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: windows::core::PWSTR(remote.as_ptr() as _),
        ..Default::default()
    };
    let code = unsafe {
        WNetAddConnection2W(
            &resource,
            windows::core::PCWSTR(pass.as_ptr()),
            windows::core::PCWSTR(user.as_ptr()),
            NET_CONNECT_FLAGS(0),
        )
    };
    match code {
        ERROR_SUCCESS => Ok(()),
        ERROR_ALREADY_ASSIGNED | ERROR_SESSION_CREDENTIAL_CONFLICT => Ok(()),
        ERROR_ACCESS_DENIED | ERROR_LOGON_FAILURE => Err(VfsError::AccessDenied(format!(
            "SMB 登录失败（用户名/密码/权限）: {target}"
        ))),
        other => Err(VfsError::Network(format!(
            "连接 {target} 失败（错误码 {}）",
            other.0
        ))),
    }
}

#[cfg(windows)]
fn wnet_disconnect(target: &str) {
    use windows::core::HSTRING;
    use windows::Win32::NetworkManagement::WNet::{WNetCancelConnection2W, NET_CONNECT_FLAGS};

    let remote = HSTRING::from(target);
    unsafe {
        let _ = WNetCancelConnection2W(
            windows::core::PCWSTR(remote.as_ptr()),
            NET_CONNECT_FLAGS(0),
            true,
        );
    }
}

#[cfg(not(windows))]
fn wnet_connect(_target: &str, _username: &str, _password: &str) -> VfsResult<()> {
    Err(VfsError::Unsupported(
        "SMB 源当前仅支持 Windows（UNC 原生客户端）".into(),
    ))
}

#[cfg(not(windows))]
fn wnet_disconnect(_target: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parses_unc_and_smb_url() {
        let (root, connect, host) = parse_smb_endpoint(r"\\nas\media").unwrap();
        assert_eq!(root, PathBuf::from(r"\\nas\media"));
        assert_eq!(connect, r"\\nas\media");
        assert_eq!(host, "nas");

        let (root, connect, _) = parse_smb_endpoint("smb://nas/media/tv").unwrap();
        assert_eq!(root, PathBuf::from(r"\\nas\media\tv"));
        assert_eq!(connect, r"\\nas\media");

        let (root, _, _) = parse_smb_endpoint(r"\\nas\media\").unwrap();
        assert_eq!(root, PathBuf::from(r"\\nas\media"));
    }

    #[test]
    fn endpoint_rejects_missing_share_and_garbage() {
        assert!(parse_smb_endpoint("smb://nas").is_err());
        assert!(parse_smb_endpoint("").is_err());
        assert!(parse_smb_endpoint("http://x/y").is_err());
        assert!(parse_smb_endpoint(r"C:\dir").is_err());
    }

    #[test]
    fn domain_joins_username() {
        assert_eq!(join_domain_user("", "u"), "u");
        assert_eq!(join_domain_user("DOM", "u"), r"DOM\u");
        assert_eq!(join_domain_user("DOM", r"DOM2\u"), r"DOM2\u");
    }

    #[test]
    fn resolve_keeps_paths_inside_root() {
        let fs = SmbFs::new(SmbConfig {
            endpoint: "smb://nas/media".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            fs.resolve(Path::new(r"\\nas\media\a\b.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\a\b.mkv")
        );
        // 相对路径拼接到根
        assert_eq!(
            fs.resolve(Path::new(r"a\b.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\a\b.mkv")
        );
        // 正斜杠规范化
        assert_eq!(
            fs.resolve(Path::new("//nas/media/x.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\x.mkv")
        );
        // 根自身
        assert_eq!(fs.resolve(Path::new(r"\\nas\media")).unwrap(), fs.root);
        // 越界（其他主机/共享）
        assert!(matches!(
            fs.resolve(Path::new(r"\\nas\other\x.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
        assert!(matches!(
            fs.resolve(Path::new(r"\\nas2\media\x.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn sub_root_restricts_scope() {
        let fs = SmbFs::new(SmbConfig {
            endpoint: "smb://nas/media/tv".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(fs.resolve(Path::new(r"\\nas\media\tv\s01\e01.mkv")).is_ok());
        assert!(matches!(
            fs.resolve(Path::new(r"\\nas\media\movies\a.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn starts_with_component_checks_boundary() {
        assert!(starts_with_component(
            Path::new(r"\\h\s\ab"),
            Path::new(r"\\h\s")
        ));
        assert!(!starts_with_component(
            Path::new(r"\\h\share2"),
            Path::new(r"\\h\share")
        ));
    }

    /// UNC 大小写不敏感：根边界判定对大小写变体等同接受
    /// （`\\NAS\MEDIA` 与 `\\nas\media` 是同一共享）。
    #[test]
    fn starts_with_component_ignores_ascii_case() {
        assert!(starts_with_component(
            Path::new(r"\\H\S\ab"),
            Path::new(r"\\h\s")
        ));
        assert!(starts_with_component(
            Path::new(r"\\h\s"),
            Path::new(r"\\H\S")
        ));
        assert!(!starts_with_component(
            Path::new(r"\\h\share2"),
            Path::new(r"\\h\SHARE")
        ));
    }

    fn media_fs() -> SmbFs {
        SmbFs::new(SmbConfig {
            endpoint: "smb://nas/media".into(),
            ..Default::default()
        })
        .unwrap()
    }

    /// 防回归：`..` 曾可绕过字符串前缀越界检查（`\\nas\media\..\other`
    /// 的 rest 以 `\` 开头被判"根内"）。词法规范化后必须拒绝。
    #[test]
    fn resolve_rejects_dotdot_escape() {
        let fs = media_fs();
        // 弹穿 share：\\nas\media\.. → \\nas\other\x 不在根内。
        assert!(matches!(
            fs.resolve(Path::new(r"\\nas\media\..\other\x.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
        // 弹穿 host：\\nas\media\..\.. 无法规范化，直接越界。
        assert!(matches!(
            fs.resolve(Path::new(r"\\nas\media\..\..\x.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
        // 相对路径 ..（拼接后弹穿根）。
        assert!(matches!(
            fs.resolve(Path::new(r"..\x.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
        assert!(matches!(
            fs.resolve(Path::new(r"..\..\x.mkv")),
            Err(VfsError::OutsideRoot { .. })
        ));
    }

    /// `..` 回退到根内路径应正常解析（与 LocalFs canonicalize 语义对齐）。
    #[test]
    fn resolve_lexically_normalizes_dotdot_within_root() {
        let fs = media_fs();
        assert_eq!(
            fs.resolve(Path::new(r"\\nas\media\a\..\b.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\b.mkv")
        );
        // 相对路径拼接后规范化。
        assert_eq!(
            fs.resolve(Path::new(r"a\..\b.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\b.mkv")
        );
        // `.` 透传（无语义段剔除）。
        assert_eq!(
            fs.resolve(Path::new(r"\\nas\media\.\x.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\x.mkv")
        );
        // 根自身与普通路径不受影响。
        assert_eq!(fs.resolve(Path::new(r"\\nas\media")).unwrap(), fs.root);
        assert_eq!(
            fs.resolve(Path::new(r"\\nas\media\a\b.mkv")).unwrap(),
            PathBuf::from(r"\\nas\media\a\b.mkv")
        );
    }

    /// 根/路径大小写变体等同接受（SMB 协议大小写不敏感；此前字符串
    /// 精确比较会把 `\\NAS\MEDIA\...` 误判 OutsideRoot）。
    #[test]
    fn resolve_accepts_case_variants_of_root() {
        let fs = media_fs();
        // 根自身的大小写变体。
        assert!(fs.resolve(Path::new(r"\\NAS\MEDIA")).is_ok());
        assert!(fs.resolve(Path::new(r"\\Nas\Media")).is_ok());
        // 根内路径的大小写变体（大小写形态保留原样返回）。
        assert_eq!(
            fs.resolve(Path::new(r"\\NAS\MEDIA\a.mkv")).unwrap(),
            PathBuf::from(r"\\NAS\MEDIA\a.mkv")
        );
        // 大小写混合 + `..` 回退组合。
        assert_eq!(
            fs.resolve(Path::new(r"\\NAS\Media\a\..\b.mkv")).unwrap(),
            PathBuf::from(r"\\NAS\Media\b.mkv")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_reports_unsupported() {
        let fs = SmbFs::new(SmbConfig {
            endpoint: "smb://nas/media".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(
            fs.list_dir(Path::new(r"\\nas\media")),
            Err(VfsError::Unsupported(_))
        ));
    }
}