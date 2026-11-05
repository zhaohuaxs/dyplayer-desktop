//! DFM+ core adapted from the MIT-licensed NipaPlay-Reload Rust implementation.
//!
//! Copyright (c) 2025 MCDFsteve.
//! Erika keeps this module as the layout/filter/retainer core and wraps it in
//! a native renderer-facing adapter instead of carrying NipaPlay's FRB/Flutter
//! presentation layer.
//!
//! Vendored into DyPlayer (crates/dyplayer-danmaku) from liberika v0.2.1
//! (https://github.com/AimesSoft/liberika, MPL-2.0). Behavioral semantics
//! are kept intact; only crate module paths were adapted.

pub mod factory;
pub mod filters;
pub mod measure;
pub mod model;
pub mod retainer;
pub mod timer;
pub mod types;
