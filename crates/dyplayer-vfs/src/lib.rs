//! dyplayer-vfs：VFS 统一文件源抽象（本地 / WebDAV / SMB）。
//!
//! M1 提供同步 LocalFs 实现；M3 新增 WebDavFs（ureq）与 SmbFs（Windows UNC）。
//! trait 保持同步语义：远程调用的延迟由上层（后台线程 + channel）吸收。

pub mod local;
pub mod media;
pub mod smb;
pub mod source;
pub mod types;
pub mod webdav;

pub use local::LocalFs;
pub use media::{
    SubtitleCandidate, extension_of, is_subtitle_file, is_video_file, match_subtitles_for,
    natural_cmp,
};
pub use smb::{SmbConfig, SmbFs};
pub use source::VfsSource;
pub use types::{RangeRead, VfsEntry, VfsError, VfsMeta, VfsResult};
pub use webdav::{WebDavConfig, WebDavFs};
