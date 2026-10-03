//! Prime Agent transcripts (`~/.prime/agent/sessions/<uuid>.jsonl`).
//!
//! Layout: `<root>/<uuid>.jsonl`.
//! Uses the Pi transcript schema with a flat root directory layout.

use std::sync::Arc;
use uniflo_core::Adapter;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(crate::pi::prime())]
}
