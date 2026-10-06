//! Uniflo engine: the harness-agnostic core.
//!
//! - [`adapter`]: the contract every harness integration implements
//! - [`jsonl`]: generic driver for append-only transcript files
//! - [`status`]: the shared work/idle state machine
//! - [`engine`]: live index, change detection and event fan-out
//! - [`update`]: crates.io version check via the system curl

pub mod adapter;
pub mod cache;
pub mod engine;
pub mod jsonl;
pub mod procs;
pub mod status;
pub mod update;
pub mod util;
mod watch;

pub use adapter::{Adapter, Cursor, HarnessInfo, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record};
pub use engine::{Engine, EngineOptions, IndexReport, Stats};
pub use jsonl::{Cx, JsonlAdapter, LineDecoder, SourceId};
pub use update::UpdateInfo;
