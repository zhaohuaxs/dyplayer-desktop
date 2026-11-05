use std::path::Path;

use crate::types::{RangeRead, VfsEntry, VfsMeta, VfsResult};

pub trait VfsSource: Send + Sync {
    fn list_dir(&self, path: &Path) -> VfsResult<Vec<VfsEntry>>;

    fn stat(&self, path: &Path) -> VfsResult<VfsMeta>;

    fn read_full(&self, path: &Path) -> VfsResult<Vec<u8>>;

    /// 随机读：从 `start` 偏移读取至多 `len` 字节（None = 读到文件尾）。
    /// `start >= 文件大小` 返回 `InvalidRange`。
    fn open_range(&self, path: &Path, start: u64, len: Option<u64>) -> VfsResult<RangeRead>;
}
