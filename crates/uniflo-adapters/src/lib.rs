//! Harness adapters. One module per transcript format, each behind a cargo feature and
//! exposing `adapters()`; [`all`] returns every compiled-in adapter.
//!
//! Adding a harness: implement [`uniflo_core::LineDecoder`] (one file = one session)
//! or [`uniflo_core::Adapter`] (anything else) in a new module, register it below,
//! add fixture tests. See `docs/adapters.md`.

#[cfg(feature = "antigravity")]
pub mod antigravity;
#[cfg(feature = "claude")]
pub mod claude;
#[cfg(feature = "cline")]
pub mod cline;
#[cfg(feature = "codebuddy")]
pub mod codebuddy;
#[cfg(feature = "codex")]
pub mod codex;
#[allow(dead_code, reason = "shared helpers; a single-adapter feature set uses only some")]
mod common;
#[cfg(feature = "copilot")]
pub mod copilot;
#[cfg(feature = "craft")]
pub mod craft;
#[cfg(feature = "cursor")]
pub mod cursor;
#[cfg(feature = "devin")]
pub mod devin;
#[cfg(feature = "dsh")]
pub mod dsh;
#[cfg(feature = "factory")]
pub mod factory;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "grok")]
pub mod grok;
#[cfg(feature = "hermes")]
pub mod hermes;
#[cfg(feature = "kimi")]
pub mod kimi;
#[cfg(feature = "kiro")]
pub mod kiro;
#[cfg(feature = "minimax")]
pub mod minimax;
#[cfg(feature = "openclaw")]
pub mod openclaw;
#[cfg(feature = "opencode")]
pub mod opencode;
#[cfg(feature = "pi")]
pub mod pi;
#[cfg(feature = "prime")]
pub mod prime;
#[cfg(feature = "reasonix")]
pub mod reasonix;
#[cfg(any(
    feature = "opencode",
    feature = "hermes",
    feature = "minimax",
    feature = "copilot",
    feature = "devin",
    feature = "openclaw"
))]
#[allow(dead_code, reason = "shared helpers; a single-adapter feature set uses only some")]
mod sqlite;
#[cfg(feature = "workbuddy")]
pub mod workbuddy;

use std::sync::Arc;
use uniflo_core::Adapter;

/// Every adapter enabled at compile time, in display order.
pub fn all() -> Vec<Arc<dyn Adapter>> {
    #[allow(unused_mut)]
    let mut v: Vec<Arc<dyn Adapter>> = Vec::new();
    #[cfg(feature = "claude")]
    v.extend(claude::adapters());
    #[cfg(feature = "codex")]
    v.extend(codex::adapters());
    #[cfg(feature = "pi")]
    v.extend(pi::adapters());
    #[cfg(feature = "prime")]
    v.extend(prime::adapters());
    #[cfg(feature = "cline")]
    v.extend(cline::adapters());
    #[cfg(feature = "gemini")]
    v.extend(gemini::adapters());
    #[cfg(feature = "antigravity")]
    v.extend(antigravity::adapters());
    #[cfg(feature = "opencode")]
    v.extend(opencode::adapters());
    #[cfg(feature = "workbuddy")]
    v.extend(workbuddy::adapters());
    #[cfg(feature = "minimax")]
    v.extend(minimax::adapters());
    #[cfg(feature = "hermes")]
    v.extend(hermes::adapters());
    #[cfg(feature = "factory")]
    v.extend(factory::adapters());
    #[cfg(feature = "reasonix")]
    v.extend(reasonix::adapters());
    #[cfg(feature = "cursor")]
    v.extend(cursor::adapters());
    #[cfg(feature = "dsh")]
    v.extend(dsh::adapters());
    #[cfg(feature = "grok")]
    v.extend(grok::adapters());
    #[cfg(feature = "kiro")]
    v.extend(kiro::adapters());
    #[cfg(feature = "kimi")]
    v.extend(kimi::adapters());
    #[cfg(feature = "codebuddy")]
    v.extend(codebuddy::adapters());
    #[cfg(feature = "copilot")]
    v.extend(copilot::adapters());
    #[cfg(feature = "devin")]
    v.extend(devin::adapters());
    #[cfg(feature = "craft")]
    v.extend(craft::adapters());
    #[cfg(feature = "openclaw")]
    v.extend(openclaw::adapters());
    v
}
