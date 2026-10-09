//! Uniflo wire schema (v1).
//!
//! The only contract shared by the daemon, the gateway and every client.
//! Everything a harness writes is normalized into [`Session`] + [`Event`], and
//! live changes travel as [`Envelope`] lines (NDJSON / SSE / WebSocket).
//!
//! Compatibility rule: v1 only grows. New optional fields and new `kind`
//! values may appear; clients must ignore what they do not know.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod search;
mod usage;
pub use usage::*;
mod agent;
pub use agent::*;

pub const SCHEMA_VERSION: u32 = 1;

/// Coarse activity of a session: the agent is either producing work or silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Idle,
    Work,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Work => "work",
        }
    }
}

/// One conversation of one harness. `key` = `"<harness>:<id>"` is globally unique.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub key: String,
    pub harness: String,
    pub id: String,
    /// Parent session key for sub-agents / forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// First human prompt, truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Where the transcript lives (file path or `sqlite://path#id`).
    pub source: String,
    /// Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    pub updated_at: i64,
    pub status: Status,
    pub status_since: i64,
    /// Why the status is what it is, e.g. `turn_end`, `tool_call`, `stale`, `live:busy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// OS process currently attached to this session, when the harness exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Token and cost totals over every `usage` event, once the usage ledger has read them.
    /// Boxed to keep `Session` (and every envelope) small; the JSON is the plain object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Box<SessionUsage>>,
}

/// One normalized transcript item.
///
/// `id` is stable inside a session: an event whose `id` was already delivered
/// **replaces** the earlier one (streaming harnesses update messages in place).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub session: String,
    /// Unix epoch milliseconds.
    pub ts: i64,
    /// Opaque paging cursor (`before=<pos>`); monotonic within a session source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos: Option<u64>,
    /// Still being streamed by the harness; a later event with the same id completes it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    /// Text fields were cut by the gateway's `max_text`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// `synthetic`: injected by the harness (command output, notifications, hook feedback), not typed by a human.
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        synthetic: bool,
    },
    AssistantMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        output: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    TurnStart {},
    TurnEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Usage(Usage),
    /// Harness-level notices: compaction, errors, mode switches, injected instructions.
    System {
        subtype: String,
        #[serde(default)]
        text: String,
    },
}

impl Body {
    pub fn kind(&self) -> &'static str {
        match self {
            Body::UserMessage { .. } => "user_message",
            Body::AssistantMessage { .. } => "assistant_message",
            Body::Reasoning { .. } => "reasoning",
            Body::ToolCall { .. } => "tool_call",
            Body::ToolResult { .. } => "tool_result",
            Body::TurnStart {} => "turn_start",
            Body::TurnEnd { .. } => "turn_end",
            Body::Usage(_) => "usage",
            Body::System { .. } => "system",
        }
    }
}

/// Tokens of one model call (one step). See `docs/schema.md#usage-口径` for the field
/// semantics every adapter normalizes to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens that missed the cache (excludes `cache_read` and `cache_write`).
    #[serde(default)]
    pub input: u64,
    /// Billed output tokens, thinking included.
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// The thinking share of `output` (informational, never billed twice).
    #[serde(default)]
    pub reasoning: u64,
    /// Model that served this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Amount the harness itself reported for this step, USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Static description of a supported harness plus live counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Harness {
    pub id: String,
    pub name: String,
    /// Storage roots that exist on this machine.
    pub roots: Vec<String>,
    pub sessions: usize,
    pub working: usize,
}

/// One line of a live stream. `seq` is global and strictly increasing per daemon run;
/// reconnect with `since=<last seq>` to replay what was missed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    Hello {
        seq: u64,
        version: u32,
        server: String,
    },
    /// Session created or its metadata/status changed (full snapshot, replace by `key`).
    Session {
        seq: u64,
        session: Session,
    },
    Event {
        seq: u64,
        event: Event,
    },
    Removed {
        seq: u64,
        key: String,
    },
    /// The subscriber fell behind and `missed` envelopes were dropped; re-sync via REST.
    Lagged {
        seq: u64,
        missed: u64,
    },
}

impl Envelope {
    pub fn seq(&self) -> u64 {
        match self {
            Envelope::Hello { seq, .. }
            | Envelope::Session { seq, .. }
            | Envelope::Event { seq, .. }
            | Envelope::Removed { seq, .. }
            | Envelope::Lagged { seq, .. } => *seq,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Envelope::Hello { .. } => "hello",
            Envelope::Session { .. } => "session",
            Envelope::Event { .. } => "event",
            Envelope::Removed { .. } => "removed",
            Envelope::Lagged { .. } => "lagged",
        }
    }

    /// Session key this envelope concerns, if any.
    pub fn session_key(&self) -> Option<&str> {
        match self {
            Envelope::Session { session, .. } => Some(&session.key),
            Envelope::Event { event, .. } => Some(&event.session),
            Envelope::Removed { key, .. } => Some(key),
            _ => None,
        }
    }
}

pub fn session_key(harness: &str, id: &str) -> String {
    format!("{harness}:{id}")
}

/// Harness id part of a session key.
pub fn harness_of(key: &str) -> &str {
    key.split_once(':').map_or(key, |(h, _)| h)
}

impl Event {
    /// Cut every free-text field to at most `max` bytes (UTF-8 safe). `0` = unlimited.
    pub fn truncate_text(&mut self, max: usize) {
        if max == 0 {
            return;
        }
        let mut cut = false;
        match &mut self.body {
            Body::UserMessage { text, .. }
            | Body::AssistantMessage { text, .. }
            | Body::Reasoning { text }
            | Body::System { text, .. } => cut |= clip(text, max),
            Body::ToolResult { output, .. } => cut |= clip(output, max),
            Body::ToolCall { input, .. } => {
                if let Value::String(s) = input {
                    cut |= clip(s, max);
                } else if approx_json_len(input, max) > max {
                    let mut s = input.to_string();
                    clip(&mut s, max);
                    *input = Value::String(s);
                    cut = true;
                }
            }
            _ => {}
        }
        self.truncated |= cut;
    }
}

/// Truncate `s` in place to at most `max` bytes on a char boundary. Returns whether it cut.
pub fn clip(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    true
}

/// Serialized-length estimate that stops early once `limit` is exceeded.
fn approx_json_len(v: &Value, limit: usize) -> usize {
    fn walk(v: &Value, acc: &mut usize, limit: usize) {
        if *acc > limit {
            return;
        }
        match v {
            Value::String(s) => *acc += s.len() + 2,
            Value::Array(a) => {
                *acc += 2;
                for x in a {
                    walk(x, acc, limit);
                }
            }
            Value::Object(o) => {
                *acc += 2;
                for (k, x) in o {
                    *acc += k.len() + 3;
                    walk(x, acc, limit);
                }
            }
            _ => *acc += 8,
        }
    }
    let mut acc = 0;
    walk(v, &mut acc, limit);
    acc
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(body: Body) -> Event {
        Event {
            id: "e1".into(),
            session: "claude:s1".into(),
            ts: 1,
            pos: Some(7),
            partial: false,
            truncated: false,
            body,
        }
    }

    #[test]
    fn event_wire_shape_is_flat_and_tagged() {
        let e = ev(Body::ToolCall { call_id: "c1".into(), name: "Bash".into(), input: json!({"command": "ls"}) });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            json!({"id":"e1","session":"claude:s1","ts":1,"pos":7,"kind":"tool_call","call_id":"c1","name":"Bash","input":{"command":"ls"}})
        );
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn every_kind_roundtrips() {
        let bodies = vec![
            Body::UserMessage { text: "hi".into(), synthetic: true },
            Body::AssistantMessage { text: "yo".into(), model: Some("m".into()) },
            Body::Reasoning { text: "r".into() },
            Body::ToolResult { call_id: "c".into(), name: None, output: "o".into(), is_error: true },
            Body::TurnStart {},
            Body::TurnEnd { reason: Some("end_turn".into()) },
            Body::Usage(Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                reasoning: 5,
                ..Default::default()
            }),
            Body::Usage(Usage {
                input: 1,
                output: 2,
                model: Some("m".into()),
                cost_usd: Some(0.25),
                ..Default::default()
            }),
            Body::System { subtype: "compact".into(), text: String::new() },
        ];
        for b in bodies {
            let e = ev(b);
            let s = serde_json::to_string(&e).unwrap();
            assert!(s.contains(&format!("\"kind\":\"{}\"", e.body.kind())), "{s}");
            assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
        }
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        let s = r#"{"id":"a","session":"x:y","ts":3,"kind":"reasoning","text":"t","future":1}"#;
        let e: Event = serde_json::from_str(s).unwrap();
        assert_eq!(e.body, Body::Reasoning { text: "t".into() });
    }

    #[test]
    fn envelope_is_type_tagged() {
        let env = Envelope::Removed { seq: 9, key: "codex:1".into() };
        assert_eq!(serde_json::to_string(&env).unwrap(), r#"{"type":"removed","seq":9,"key":"codex:1"}"#);
        assert_eq!(env.seq(), 9);
        assert_eq!(env.session_key(), Some("codex:1"));
    }

    #[test]
    fn truncate_is_utf8_safe_and_flags() {
        let mut e = ev(Body::AssistantMessage { text: "你好世界".into(), model: None });
        e.truncate_text(4);
        assert_eq!(e.body, Body::AssistantMessage { text: "你".into(), model: None });
        assert!(e.truncated);

        let mut e = ev(Body::ToolCall { call_id: "c".into(), name: "n".into(), input: json!({"k": "x".repeat(100)}) });
        e.truncate_text(20);
        assert!(matches!(&e.body, Body::ToolCall { input: Value::String(s), .. } if s.len() <= 20));

        let mut e = ev(Body::Reasoning { text: "short".into() });
        e.truncate_text(100);
        assert!(!e.truncated);
    }

    #[test]
    fn usage_grows_optional_model_and_cost() {
        let old = r#"{"id":"u","session":"x:y","ts":1,"kind":"usage","input":3,"output":4}"#;
        let e: Event = serde_json::from_str(old).unwrap();
        assert_eq!(e.body, Body::Usage(Usage { input: 3, output: 4, ..Default::default() }));
        assert_eq!(serde_json::to_value(&e).unwrap()["model"], Value::Null, "absent fields stay absent");
        let e = ev(Body::Usage(Usage { input: 1, model: Some("m".into()), cost_usd: Some(0.5), ..Default::default() }));
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["model"].as_str(), v["cost_usd"].as_f64()),
            (Some("usage"), Some("m"), Some(0.5))
        );
    }

    #[test]
    fn session_usage_is_optional() {
        let s = r#"{"key":"x:y","harness":"x","id":"y","source":"/s","updated_at":1,"status":"idle","status_since":0}"#;
        let sess: Session = serde_json::from_str(s).unwrap();
        assert!(sess.usage.is_none());
        assert!(!serde_json::to_string(&sess).unwrap().contains("usage"));
        let u = SessionUsage { steps: 2, unpriced_steps: 1, cost_usd: None, ..Default::default() };
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["cost_usd"], Value::Null, "unpriced totals are null, not 0");
        let m = ModelUsage { model: "m".into(), match_kind: Some(MatchKind::Approx), ..Default::default() };
        assert_eq!(serde_json::to_value(&m).unwrap()["match"], "approx");
    }

    #[test]
    fn key_helpers() {
        assert_eq!(session_key("pi", "abc"), "pi:abc");
        assert_eq!(harness_of("pi:abc:def"), "pi");
    }
}
