//! Optional cross-agent messaging types (ADR-0012).
//!
//! Pure additive v1 wire types for `/v1/messages`, `/v1/messages/ack`, and `/v1/messages/stream`.

use crate::Status;
use serde::{Deserialize, Serialize};

/// One message routed between local agent sessions through Uniflo's optional message bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    /// Sender session key (`<harness>:<id>`) or caller label (e.g. `cli`).
    pub from: String,
    /// Resolved recipient session key (`<harness>:<id>`).
    pub to: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Unix epoch milliseconds.
    pub created_at: i64,
    /// Set when the recipient pulled/injected the message (e.g. via hook).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<i64>,
    /// Set when explicitly acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<i64>,
}

/// Snapshot of the resolved recipient session at send time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTarget {
    pub key: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Suggested delivery path: `"hook"` (working), `"pty_or_hook"` (idle with live pid),
    /// or `"resume"` (offline).
    pub delivery: String,
    /// Command that resumes the session in its harness, when supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
}

/// Body of `POST /v1/messages`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    /// Exact session key, unique key/id prefix, or `cwd:<dir>` / directory path.
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Optional active delivery mode: `"auto"` (Herdr/tmux live injection or headless resume when idle),
    /// `"wake"` (always attempt live injection or headless resume), or `"inbox"` (store & SSE only; default when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<String>,
    /// Optional seconds (1–90) to wait inline for the target session's reply when waking/delivering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<u64>,
    /// Auto-wake hop counter used to prevent infinite AI-to-AI wake loops (capped server-side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop: Option<u8>,
}

/// Response of `POST /v1/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub message: AgentMessage,
    pub target: MessageTarget,
    /// Actual delivery channel used when active delivery ran (`"herdr"`, `"tmux"`, `"resume_wake"`,
    /// `"resume_failed"`, `"inbox"`, `"inbox_busy"`, `"loop_guard"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_channel: Option<String>,
    /// Human-readable delivery summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_detail: Option<String>,
    /// Captured reply message when `wait_secs` received a response within the timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<AgentMessage>,
}

/// One reachable local agent peer returned by `GET /v1/messages/peers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub key: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub updated_at: i64,
    /// Best zero-hook delivery channel currently available: `"herdr"`, `"tmux"`, `"resume_wake"`, or `"inbox"`.
    pub channel: String,
    /// Matched live Herdr terminal address or tmux pane id, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    pub unread: usize,
}

/// Response of `GET /v1/messages/peers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeersResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_key: Option<String>,
    pub peers: Vec<PeerInfo>,
}

/// Response of `GET /v1/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub unread: usize,
}

/// Body of `POST /v1/messages/ack`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckMessagesRequest {
    #[serde(default)]
    pub ids: Vec<String>,
    /// Mark all matching messages for this recipient session key (or resolved target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// `"delivered"` (hook pull) or `"acked"` (default; sets both `delivered_at` and `acked_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Response of `POST /v1/messages/ack`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckMessagesResponse {
    pub updated: usize,
}
