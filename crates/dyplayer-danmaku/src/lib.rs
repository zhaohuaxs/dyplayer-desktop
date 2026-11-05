//! DFM+ 弹幕布局内核（vendored）+ XML 规范化 + glyph atlas 自绘渲染，M2 实现。
//!
//! vendored 布局层（vendor/ 目录）保持上游 v0.2.1（MPL-2.0）行为语义不动：
//! - `dfm`：prepare_layout / for_each_frame_item 契约适配层
//! - `dfm_core`：派生自上游 MIT 布局内核的过滤/轨道分配实现
//! - `outline` / `typography`：描边宽度与字号策略
//!
//! 自有层（M2）：
//! - `normalize`：B站/弹弹play XML 规范化
//! - `session` / `layout`：generation 管理 + 后台 prepare（媒体时间由调用方
//!   从 dyplayer-mpv 的 MediaClock 查询后传入，弹幕侧无独立时钟，TDD 6.3 单源契约）
//! - `render`：glyph atlas + epaint Mesh 自绘

#[path = "../vendor/dfm.rs"]
pub mod dfm;

#[path = "../vendor/dfm_core/mod.rs"]
pub mod dfm_core;

#[path = "../vendor/outline.rs"]
mod outline;

#[path = "../vendor/typography.rs"]
mod typography;

pub mod layout;
pub mod normalize;
pub mod render;
pub mod session;
