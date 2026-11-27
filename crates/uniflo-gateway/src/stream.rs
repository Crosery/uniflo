//! One envelope stream shared by SSE, NDJSON and WebSocket transports.

use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use uniflo_core::Engine;
use uniflo_schema::{Body, Envelope, Event, harness_of};

#[derive(Debug, Default, Deserialize)]
pub struct StreamParams {
    pub since: Option<u64>,
    /// Comma-separated session keys.
    pub session: Option<String>,
    /// Comma-separated harness ids.
    pub harness: Option<String>,
    /// Comma-separated envelope types (`session,event,removed`).
    pub types: Option<String>,
    /// Comma-separated event kinds (`tool_call,assistant_message,…`).
    pub kinds: Option<String>,
    pub max_text: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    sessions: Option<HashSet<String>>,
    harnesses: Option<HashSet<String>>,
    types: Option<HashSet<String>>,
    kinds: Option<HashSet<String>>,
    max_text: usize,
}

fn set(v: &Option<String>) -> Option<HashSet<String>> {
    v.as_deref().map(|s| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect())
}

impl From<&StreamParams> for Filter {
    fn from(p: &StreamParams) -> Self {
        Filter {
            sessions: set(&p.session),
            harnesses: set(&p.harness),
            types: set(&p.types),
            kinds: set(&p.kinds),
            max_text: p.max_text.unwrap_or(crate::DEFAULT_MAX_TEXT),
        }
    }
}

impl Filter {
    pub fn keep(&self, env: &Envelope) -> bool {
        if matches!(env, Envelope::Hello { .. } | Envelope::Lagged { .. }) {
            return true;
        }
        if self.types.as_ref().is_some_and(|t| !t.contains(env.type_name())) {
            return false;
        }
        if let Some(key) = env.session_key() {
            if self.sessions.as_ref().is_some_and(|s| !s.contains(key)) {
                return false;
            }
            if self.harnesses.as_ref().is_some_and(|h| !h.contains(harness_of(key))) {
                return false;
            }
        }
        if let (Some(kinds), Envelope::Event { event, .. }) = (&self.kinds, env) {
            return kinds.contains(event.body.kind());
        }
        true
    }

    fn shape(&self, env: Arc<Envelope>) -> Arc<Envelope> {
        match &*env {
            Envelope::Event { seq, event } if self.max_text > 0 && needs_truncate(event, self.max_text) => {
                let mut e = event.clone();
                e.truncate_text(self.max_text);
                if e.truncated { Arc::new(Envelope::Event { seq: *seq, event: e }) } else { env }
            }
            _ => env,
        }
    }
}

/// Read-only mirror of `Event::truncate_text` so the 99% non-truncated stream fast path avoids
/// cloning every `Event` on every subscriber.
fn needs_truncate(event: &Event, max: usize) -> bool {
    if max == 0 {
        return false;
    }
    match &event.body {
        Body::UserMessage { text, .. }
        | Body::AssistantMessage { text, .. }
        | Body::Reasoning { text }
        | Body::System { text, .. } => text.len() > max,
        Body::ToolResult { output, .. } => output.len() > max,
        Body::ToolCall { input, .. } => match input {
            Value::String(s) => s.len() > max,
            other => approx_json_exceeds(other, max),
        },
        _ => false,
    }
}

fn approx_json_exceeds(v: &Value, limit: usize) -> bool {
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
    acc > limit
}

/// `hello`, then (when `since` is given) the replay backlog or a `lagged` marker, then live envelopes.
pub fn envelopes(
    engine: &Engine,
    since: Option<u64>,
    filter: Filter,
) -> impl Stream<Item = Arc<Envelope>> + Send + use<> {
    let (backlog, rx, complete) = engine.subscribe(since);
    let mut head = vec![Arc::new(engine.hello())];
    if !complete {
        head.push(Arc::new(Envelope::Lagged { seq: since.unwrap_or(0), missed: 0 }));
    }
    let last = backlog.last().map(|e| e.seq()).or(since).unwrap_or(0);
    head.extend(backlog);
    let live = stream::unfold((rx, last), |(mut rx, last)| async move {
        loop {
            match rx.recv().await {
                Ok(env) if env.seq() <= last => continue,
                Ok(env) => {
                    let seq = env.seq();
                    return Some((env, (rx, seq)));
                }
                Err(RecvError::Lagged(n)) => {
                    return Some((Arc::new(Envelope::Lagged { seq: last, missed: n }), (rx, last)));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });
    let f = filter.clone();
    stream::iter(head).chain(live).filter(move |e| std::future::ready(f.keep(e))).map(move |e| filter.shape(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_event(body: Body) -> Arc<Envelope> {
        Arc::new(Envelope::Event {
            seq: 1,
            event: Event {
                id: "e1".into(),
                session: "claude:s1".into(),
                ts: 1,
                pos: None,
                partial: false,
                truncated: false,
                body,
            },
        })
    }

    #[test]
    fn shape_avoids_cloning_short_events_and_truncates_oversized_ones() {
        let f = Filter { max_text: 16, ..Default::default() };

        let short = mk_event(Body::AssistantMessage { text: "hello".into(), model: None });
        let shaped = f.shape(short.clone());
        assert!(Arc::ptr_eq(&short, &shaped), "short event should pass through zero-clone");

        let long = mk_event(Body::AssistantMessage { text: "x".repeat(64), model: None });
        let shaped_long = f.shape(long.clone());
        assert!(!Arc::ptr_eq(&long, &shaped_long));
        match &*shaped_long {
            Envelope::Event { event, .. } => {
                assert!(event.truncated);
                assert!(matches!(&event.body, Body::AssistantMessage { text, .. } if text.len() == 16));
            }
            _ => panic!("expected event"),
        }

        let small_tool = mk_event(Body::ToolCall { call_id: "c1".into(), name: "ls".into(), input: json!({"a": 1}) });
        assert!(Arc::ptr_eq(&small_tool, &f.shape(small_tool.clone())));

        let big_tool = mk_event(Body::ToolCall {
            call_id: "c2".into(),
            name: "write".into(),
            input: json!({"content": "y".repeat(64)}),
        });
        let shaped_tool = f.shape(big_tool.clone());
        assert!(!Arc::ptr_eq(&big_tool, &shaped_tool));
        assert!(matches!(&*shaped_tool, Envelope::Event { event, .. } if event.truncated));
    }
}
