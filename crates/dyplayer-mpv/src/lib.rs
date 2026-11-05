//! dyplayer-mpv：libmpv render API 内嵌引擎（M0 PoC）。

pub mod client;
pub mod clock;
pub mod events;
pub mod render;
mod ffi;
mod node;

pub use client::{Chapter, ClientOptions, MpvClient, ObsFormat, Track};
pub use clock::{ClockSnapshot, MediaClock};
pub use events::{EndFileReason, EngineEvent, PropertyChange};
pub use ffi::{FfiError, MPV_RENDER_UPDATE_FRAME, MpvLibrary};
pub use render::{FrameInfo, MpvRenderContext};
