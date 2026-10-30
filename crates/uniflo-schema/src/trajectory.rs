//! The call chain of one session served by `GET /v1/sessions/{key}/trajectory`: turns, the
//! model calls (steps) in them, and what each step did (items), plus the sub-sessions it started.

use crate::{CostParts, CostSource, SessionFamily, SessionUsage, Status, StepPrice};
use serde::{Deserialize, Serialize};

/// `GET /v1/sessions/{key}/trajectory`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTrajectory {
    pub session: String,
    pub turns: Vec<TrajectoryTurn>,
    /// Direct sub-sessions, by start time.
    pub children: Vec<TrajectoryChild>,
}

/// One turn: opened by a human message (harness-injected messages do not open one). Turn `0`
/// holds what happened before the first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryTurn {
    /// Same numbering as `StepUsage.turn` of `/usage`.
    pub turn: u32,
    /// Event id of the opening human message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub started_at: i64,
    /// Latest record of the turn.
    pub ended_at: i64,
    /// First 300 characters of the human message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub steps: Vec<TrajectoryStep>,
    /// Records no step claims: those at the end of the turn, after its last usage.
    pub items: Vec<TrajectoryItem>,
}

/// One model call; the numbers are those of the same step in `GET /v1/sessions/{key}/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryStep {
    /// Position in the session, from 1; the step's place in `/usage`.
    pub index: u32,
    /// Event id of the `usage` event.
    pub event: String,
    /// End of the previous step's last record, or the turn's start for its first step.
    pub started_at: i64,
    /// Time of the `usage` event.
    pub ts: i64,
    #[serde(default)]
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub cost_source: Option<CostSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_parts: Option<CostParts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<StepPrice>,
    pub context_tokens: u64,
    #[serde(default)]
    pub context_pct: Option<f64>,
    /// Records between the previous usage and this one, in order.
    pub items: Vec<TrajectoryItem>,
}

/// One record of a step. Every kind carries the source `event` id and its `ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrajectoryItem {
    Reasoning {
        event: String,
        ts: i64,
        chars: u64,
    },
    Text {
        event: String,
        ts: i64,
        chars: u64,
        /// First 200 characters.
        preview: String,
    },
    Tool {
        event: String,
        ts: i64,
        name: String,
        call_id: String,
        /// First 200 characters of the arguments.
        input_preview: String,
        /// When the result arrived.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        /// First 200 characters of the output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
        /// No result yet.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        running: bool,
        /// Sub-sessions this call started (session keys).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<String>,
    },
    System {
        event: String,
        ts: i64,
        subtype: String,
    },
}

/// A direct sub-session of the traced session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryChild {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SessionUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<SessionFamily>,
    /// The tool call that started it (`call_id` of a `tool` item).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// `index` of the step it hangs from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
}
