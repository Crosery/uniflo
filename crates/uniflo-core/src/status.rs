//! Work/idle state machine shared by every harness.
//!
//! Adapters only translate records; status is derived here from the normalized event
//! sequence, so one rule set covers all harnesses:
//!
//! - user message, reasoning, assistant text, tool call/result, turn start, partial event → `work`
//! - turn end → `idle`
//! - usage / system notices → no change
//!
//! On top of that the engine applies liveness (process gone → idle) and staleness
//! (`work` with no activity for `stale_after` and no live process → idle).

use serde::{Deserialize, Serialize};
use uniflo_schema::{Body, Event, Status};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusTracker {
    pub status: Status,
    /// When `status` last changed (epoch ms).
    pub since: i64,
    pub reason: String,
    /// Timestamp of the latest activity-bearing event.
    pub last: i64,
}

impl StatusTracker {
    /// Feed one event in transcript order. Returns true when the status flipped.
    pub fn observe(&mut self, e: &Event) -> bool {
        let Some((next, reason)) = transition(e) else {
            return false;
        };
        let ts = e.ts.max(self.last);
        self.last = ts;
        if next != self.status || self.reason.is_empty() {
            let flipped = next != self.status;
            self.status = next;
            self.reason = reason.to_owned();
            if flipped || self.since == 0 {
                self.since = ts;
            }
            return flipped;
        }
        self.reason = reason.to_owned();
        false
    }

    /// Force a status (process exit, explicit harness state).
    pub fn force(&mut self, status: Status, reason: &str, at: i64) {
        if self.status != status {
            self.since = at;
        }
        self.status = status;
        self.reason = reason.to_owned();
    }
}

fn transition(e: &Event) -> Option<(Status, &'static str)> {
    if e.partial {
        return Some((Status::Work, "streaming"));
    }
    Some(match &e.body {
        Body::UserMessage { .. } => (Status::Work, "user_message"),
        Body::Reasoning { .. } => (Status::Work, "reasoning"),
        Body::AssistantMessage { .. } => (Status::Work, "assistant_message"),
        Body::ToolCall { .. } => (Status::Work, "tool_call"),
        Body::ToolResult { .. } => (Status::Work, "tool_result"),
        Body::TurnStart {} => (Status::Work, "turn_start"),
        Body::TurnEnd { .. } => (Status::Idle, "turn_end"),
        Body::Usage(_) | Body::System { .. } => return None,
    })
}

/// Effective status after liveness and staleness rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub status: Status,
    pub since: i64,
    pub reason: String,
}

/// Silence windows after which `work` without a live process is considered over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Windows {
    /// Waiting on a tool or the model (long tool runs are normal).
    pub stale_ms: i64,
    /// Last activity was assistant text: either the turn is over or more blocks follow soon.
    pub settle_ms: i64,
}

pub fn effective(t: &StatusTracker, live_pid: bool, live_status: Option<Status>, now: i64, w: Windows) -> Effective {
    if let Some(s) = live_status {
        return Effective { status: s, since: t.since, reason: "live".into() };
    }
    let window = if t.reason == "assistant_message" { w.settle_ms.min(w.stale_ms) } else { w.stale_ms };
    if t.status == Status::Work && !live_pid && window > 0 && now - t.last > window {
        return Effective { status: Status::Idle, since: t.last + window, reason: "stale".into() };
    }
    Effective { status: t.status, since: t.since, reason: t.reason.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: i64, body: Body) -> Event {
        Event { id: format!("e{ts}"), session: "x:1".into(), ts, pos: None, partial: false, truncated: false, body }
    }

    #[test]
    fn full_turn_cycle() {
        let mut t = StatusTracker::default();
        assert!(t.observe(&ev(10, Body::UserMessage { text: "hi".into(), synthetic: false })));
        assert_eq!((t.status, t.since), (Status::Work, 10));
        assert!(
            !t.observe(&ev(20, Body::ToolCall { call_id: "c".into(), name: "n".into(), input: Default::default() }))
        );
        assert_eq!(t.since, 10, "since only moves on flips");
        assert_eq!(t.reason, "tool_call");
        assert!(!t.observe(&ev(25, Body::Usage(Default::default()))));
        assert_eq!(t.last, 20, "usage is not activity");
        assert!(t.observe(&ev(30, Body::TurnEnd { reason: None })));
        assert_eq!((t.status, t.since, t.reason.as_str()), (Status::Idle, 30, "turn_end"));
    }

    #[test]
    fn partial_events_mean_work() {
        let mut t = StatusTracker::default();
        let mut e = ev(5, Body::TurnEnd { reason: None });
        e.partial = true;
        t.observe(&e);
        assert_eq!(t.status, Status::Work);
    }

    #[test]
    fn out_of_order_timestamps_never_go_back() {
        let mut t = StatusTracker::default();
        t.observe(&ev(100, Body::UserMessage { text: String::new(), synthetic: false }));
        t.observe(&ev(0, Body::TurnEnd { reason: None }));
        assert_eq!((t.last, t.since), (100, 100));
    }

    #[test]
    fn staleness_and_liveness() {
        let mut t = StatusTracker::default();
        t.observe(&ev(1_000, Body::ToolCall { call_id: "c".into(), name: "n".into(), input: Default::default() }));
        let w = Windows { stale_ms: 60_000, settle_ms: 5_000 };
        let stale = effective(&t, false, None, 1_000 + 60_001, w);
        assert_eq!((stale.status, stale.since, stale.reason.as_str()), (Status::Idle, 61_000, "stale"));
        let alive = effective(&t, true, None, 1_000 + 60_001, w);
        assert_eq!(alive.status, Status::Work, "a live process keeps a long tool call working");
        let fresh = effective(&t, false, None, 2_000, w);
        assert_eq!(fresh.status, Status::Work);
        let forced = effective(&t, true, Some(Status::Idle), 2_000, w);
        assert_eq!(forced.status, Status::Idle);
        t.observe(&ev(2_000, Body::AssistantMessage { text: "done?".into(), model: None }));
        assert_eq!(effective(&t, false, None, 2_000 + 5_001, w).status, Status::Idle, "text settles fast");
        assert_eq!(effective(&t, false, None, 2_000 + 4_000, w).status, Status::Work);
        assert_eq!(effective(&t, true, None, 2_000 + 50_000, w).status, Status::Work, "live pid disables settling");
    }
}
