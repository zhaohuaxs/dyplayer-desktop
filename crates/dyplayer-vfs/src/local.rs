use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::source::VfsSource;
use crate::types::{RangeRead, VfsEntry, VfsError, VfsMeta, VfsResult};

pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: &Path) -> VfsResult<Self> {
        let canonical = std::fs::canonicalize(root)?;
        if !canonical.is_dir() {
            return Err(VfsError::NotFound(canonical.display().to_string()));
        }
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &Path) -> VfsResult<PathBuf> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let canonical = std::fs::canonicalize(&candidate).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                VfsError::NotFound(candidate.display().to_string())
            } else {
                VfsError::from(e)
            }
        })?;
        if canonical != self.root && !canonical.starts_with(&self.root) {
            return Err(VfsError::OutsideRoot {
                root: self.root.display().to_string(),
                path: canonical.display().to_string(),
            });
        }
        Ok(canonical)
    }
}

impl VfsSource for LocalFs {
    fn list_dir(&self, path: &Path) -> VfsResult<Vec<VfsEntry>> {
        let dir = self.resolve(path)?;
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&dir)? {
            let item = item?;
            let Ok(file_type) = item.file_type() else {
                continue;
            };
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            let is_dir = file_type.is_dir();
            if !is_dir && file_type.is_symlink() {
                continue;
            }
            let (size, modified) = match item.metadata() {
                Ok(meta) => (meta.len(), meta.modified().ok()),
                Err(_) => (0, None),
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

    fn stat(&self, path: &Path) -> VfsResult<VfsMeta> {
        let real = self.resolve(path)?;
        let meta = std::fs::metadata(&real)?;
        Ok(VfsMeta {
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn read_full(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let real = self.resolve(path)?;
        Ok(std::fs::read(&real)?)
    }

    fn open_range(&self, path: &Path, start: u64, len: Option<u64>) -> VfsResult<RangeRead> {
        let real = self.resolve(path)?;
        let mut file = std::fs::File::open(&real)?;
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VfsError;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dyplayer_vfs_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_dir_returns_entries() {
        let root = temp_root("list");
        std::fs::write(root.join("a.mp4"), b"x").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let fs = LocalFs::new(&root).unwrap();
        let mut entries = fs.list_dir(&root).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.mp4");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 1);
        assert_eq!(entries[1].name, "sub");
        assert!(entries[1].is_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_paths_outside_root() {
        let root = temp_root("outside");
        let fs = LocalFs::new(&root).unwrap();
        let outside = std::env::temp_dir();
        match fs.list_dir(&outside) {
            Err(VfsError::OutsideRoot { .. }) => {}
            other => panic!("expected OutsideRoot, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_path_maps_to_not_found() {
        let root = temp_root("missing");
        let fs = LocalFs::new(&root).unwrap();
        match fs.stat(&root.join("nope.mp4")) {
            Err(VfsError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn read_all(mut r: RangeRead) -> Vec<u8> {
        let mut buf = Vec::new();
        r.reader.read_to_end(&mut buf).unwrap();
        buf
    }

    #[test]
    fn open_range_reads_suffix_when_len_none() {
        let root = temp_root("range");
        std::fs::write(root.join("f.bin"), b"0123456789").unwrap();
        let fs = LocalFs::new(&root).unwrap();
        let r = fs.open_range(&root.join("f.bin"), 2, None).unwrap();
        assert_eq!(r.length, 8);
        assert_eq!(read_all(r), b"23456789");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_range_clamps_len_to_file_tail() {
        let root = temp_root("range2");
        std::fs::write(root.join("f.bin"), b"0123456789").unwrap();
        let fs = LocalFs::new(&root).unwrap();
        let r = fs.open_range(&root.join("f.bin"), 2, Some(100)).unwrap();
        assert_eq!(r.length, 8);
        assert_eq!(read_all(r), b"23456789");
        let r = fs.open_range(&root.join("f.bin"), 4, Some(3)).unwrap();
        assert_eq!(r.length, 3);
        assert_eq!(read_all(r), b"456");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_range_rejects_start_beyond_size() {
        let root = temp_root("range3");
        std::fs::write(root.join("f.bin"), b"0123456789").unwrap();
        let fs = LocalFs::new(&root).unwrap();
        match fs.open_range(&root.join("f.bin"), 20, None) {
            Err(VfsError::InvalidRange { start, total }) => {
                assert_eq!((start, total), (20, 10));
            }
            other => panic!("expected InvalidRange, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
