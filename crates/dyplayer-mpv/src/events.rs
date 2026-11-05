use std::ffi::c_int;

#[derive(Debug, Clone)]
pub enum EngineEvent {
    StartFile {
        entry_id: i64,
    },
    FileLoaded,
    VideoReconfig,
    EndFile {
        reason: EndFileReason,
        error: Option<String>,
    },
    Seek,
    PlaybackRestart,
    PropertyChange(PropertyChange),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndFileReason {
    Eof,
    Stop,
    Quit,
    Error,
    Redirect,
    Unknown,
}

impl EndFileReason {
    pub fn from_raw(raw: c_int) -> Self {
        match raw {
            crate::ffi::MPV_END_FILE_REASON_EOF => Self::Eof,
            crate::ffi::MPV_END_FILE_REASON_STOP => Self::Stop,
            crate::ffi::MPV_END_FILE_REASON_QUIT => Self::Quit,
            crate::ffi::MPV_END_FILE_REASON_ERROR => Self::Error,
            crate::ffi::MPV_END_FILE_REASON_REDIRECT => Self::Redirect,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PropertyChange {
    TimePos(f64),
    Duration(f64),
    Speed(f64),
    Paused(bool),
    /// demuxer-cache-time：已缓冲的前沿时长（秒）。
    DemuxerCacheTime(f64),
    /// paused-for-cache：网络缓冲停滞（R04）。
    PausedForCache(bool),
    /// core-idle：播放核心空闲（缓冲/加载/EOF/错误，R04 时钟停滞驱动）。
    CoreIdle(bool),
    Other { name: String },
}
