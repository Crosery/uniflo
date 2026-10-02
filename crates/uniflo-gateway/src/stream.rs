//! One envelope stream shared by SSE, NDJSON and WebSocket transports.

use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use uniflo_core::Engine;
use uniflo_schema::{Envelope, harness_of};

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
            Envelope::Event { seq, event } if self.max_text > 0 => {
                let mut e = event.clone();
                e.truncate_text(self.max_text);
                if e.truncated { Arc::new(Envelope::Event { seq: *seq, event: e }) } else { env }
            }
            _ => env,
        }
    }
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
