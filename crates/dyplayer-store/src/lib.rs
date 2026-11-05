//! dyplayer-store：SQLite 媒体库 + config.toml 配置 + keyring 凭证（M0 骨架）。

pub mod config;
pub mod db;
pub mod secrets;

pub use config::{
    AppConfig, DanmakuConfig, DanmakuFontChoice, EngineConfig, FontChoice, PlayerConfig,
    StartupView, ThemeMode, UiConfig,
};
pub use db::{FavoriteEntry, IndexHit, MediaLibEntry, PlayRecord, RecentEntry, Store, TrackKind};
