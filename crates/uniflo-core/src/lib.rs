//! Uniflo engine: the harness-agnostic core.
//!
//! - [`adapter`]: the contract every harness integration implements
//! - [`jsonl`]: generic driver for append-only transcript files
//! - [`status`]: the shared work/idle state machine
//! - [`engine`]: live index, change detection and event fan-out
//! - [`update`]: crates.io version check via the system curl
//! - [`pricing`]: model price catalog (snapshot + sync + overrides) and per-step cost
//! - [`usage`]: per-step usage ledgers and their aggregations
//! - [`paths`]: Uniflo's own data / config / cache directories
//! - [`window`]: event windows centred on one event (search hit → transcript)
//! - [`cleanup`]: user-confirmed session cleanup (plan → archive → trash); [`archive`] keeps
//!   the compact transcripts of cleaned-up sessions
//! - [`resume`], [`memory`], [`context`]: resume commands, agent memory / instruction files and
//!   the recent-session hand-off for agents

pub mod adapter;
pub mod archive;
pub mod cache;
pub mod cleanup;
pub mod context;
pub mod engine;
pub mod jsonl;
pub mod memory;
pub mod paths;
pub mod pricing;
pub mod procs;
pub mod resume;
pub mod status;
pub mod update;
pub mod usage;
pub mod util;
mod watch;
pub mod window;

pub use adapter::{Adapter, Cursor, HarnessInfo, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record};
pub use engine::{Engine, EngineOptions, IndexReport, PriceSync, Stats};
pub use jsonl::{Cx, JsonlAdapter, LineDecoder, SourceId, decode_record};
pub use update::UpdateInfo;
pub use window::Window;
