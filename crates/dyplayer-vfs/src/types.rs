use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug)]
pub enum VfsError {
    NotFound(String),
    AccessDenied(String),
    OutsideRoot { root: String, path: String },
    InvalidRange { start: u64, total: u64 },
    Network(String),
    Unsupported(String),
    Io(std::io::Error),
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "路径不存在: {p}"),
            Self::AccessDenied(p) => write!(f, "访问被拒绝: {p}"),
            Self::OutsideRoot { root, path } => {
                write!(f, "路径越界: {path} 不在根 {root} 内")
            }
            Self::InvalidRange { start, total } => {
                write!(f, "无效范围: 起始 {start} 超出文件大小 {total}")
            }
            Self::Network(e) => write!(f, "网络错误: {e}"),
            Self::Unsupported(msg) => write!(f, "不支持的操作: {msg}"),
            Self::Io(e) => write!(f, "IO 错误: {e}"),
        }
    }
}

impl std::error::Error for VfsError {}

impl From<std::io::Error> for VfsError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(e.to_string()),
            std::io::ErrorKind::PermissionDenied => Self::AccessDenied(e.to_string()),
            _ => Self::Io(e),
        }
    }
}

pub type VfsResult<T> = Result<T, VfsError>;

#[derive(Debug, Clone)]
pub struct VfsEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub struct VfsMeta {
    pub size: u64,
    pub modified: Option<SystemTime>,
}
/// 随机读结果：`reader` 最多可读 `length` 字节（HTTP Content-Length 语义）。
pub struct RangeRead {
    pub reader: Box<dyn std::io::Read + Send>,
    pub length: u64,
}
impl fmt::Debug for RangeRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RangeRead").field("length", &self.length).finish()
    }
}
