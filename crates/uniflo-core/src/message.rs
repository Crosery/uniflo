//! Optional local cross-agent message bus (ADR-0012).
//!
//! Keeps messages in memory behind an `RwLock` for sub-millisecond inbox reads, broadcasts new
//! messages over `tokio::sync::broadcast` for zero-polling SSE subscribers, and lazily appends to
//! `<data_dir>/messages/log.jsonl` on first write (creating nothing on disk if unused).
//! Harness session files remain strictly read-only.

use crate::engine::Engine;
use crate::resume;
use crate::util::{home, now_ms};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::broadcast;
use uniflo_schema::{
    AckMessagesRequest, AckMessagesResponse, AgentMessage, InboxResponse, MessageTarget, SendMessageRequest,
    SendMessageResponse, Session, Status,
};

/// Maximum message body size (64 KiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Maximum retained messages in memory (oldest evicted first).
const MAX_RETAINED: usize = 10_000;
const BROADCAST_CAP: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageError {
    BadRequest(String),
    NotFound(String),
    Ambiguous(String, Vec<String>),
    Internal(String),
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageError::BadRequest(m)
            | MessageError::NotFound(m)
            | MessageError::Ambiguous(m, _)
            | MessageError::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for MessageError {}

#[derive(Debug, Clone, Default)]
pub struct InboxQuery {
    /// Exact session key, unique prefix, or `cwd:<dir>`.
    pub to: Option<String>,
    /// Match messages addressed to OR sent from this session key (two-way conversation view).
    pub peer: Option<String>,
    /// Match messages addressed to any session whose `cwd` matches this directory.
    pub cwd: Option<String>,
    pub from: Option<String>,
    /// Only messages not yet acknowledged (`acked_at.is_none()`).
    pub unread: bool,
    /// Only messages not yet pulled/delivered (`delivered_at.is_none()`).
    pub undelivered: bool,
    pub limit: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum JournalEntry {
    Send {
        seq: u64,
        msg: AgentMessage,
    },
    Mark {
        ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acked_at: Option<i64>,
    },
}

#[derive(Default)]
struct Store {
    next_seq: u64,
    messages: BTreeMap<u64, AgentMessage>,
    by_id: HashMap<String, u64>,
    by_target: HashMap<String, Vec<u64>>,
    journal_lines: usize,
}

impl Store {
    fn insert(&mut self, seq: u64, msg: AgentMessage) {
        self.next_seq = self.next_seq.max(seq + 1);
        self.by_id.insert(msg.id.clone(), seq);
        self.by_target.entry(msg.to.clone()).or_default().push(seq);
        self.messages.insert(seq, msg);
        while self.messages.len() > MAX_RETAINED {
            let Some((&old_seq, _)) = self.messages.first_key_value() else { break };
            if let Some(old) = self.messages.remove(&old_seq) {
                self.by_id.remove(&old.id);
                if let Some(list) = self.by_target.get_mut(&old.to) {
                    list.retain(|&s| s != old_seq);
                    if list.is_empty() {
                        self.by_target.remove(&old.to);
                    }
                }
            }
        }
    }

    fn apply_mark(&mut self, ids: &[String], delivered_at: Option<i64>, acked_at: Option<i64>) -> usize {
        let mut updated = 0usize;
        for id in ids {
            let Some(&seq) = self.by_id.get(id) else { continue };
            let Some(m) = self.messages.get_mut(&seq) else { continue };
            let mut changed = false;
            if let Some(ts) = delivered_at
                && m.delivered_at.is_none()
            {
                m.delivered_at = Some(ts);
                changed = true;
            }
            if let Some(ts) = acked_at
                && m.acked_at.is_none()
            {
                m.acked_at = Some(ts);
                if m.delivered_at.is_none() {
                    m.delivered_at = Some(ts);
                }
                changed = true;
            }
            if changed {
                updated += 1;
            }
        }
        updated
    }
}

/// Shared local message bus attached to the daemon (lazy disk journal, zero files if unused).
pub struct MessageBus {
    path: Option<PathBuf>,
    store: RwLock<Store>,
    io_lock: Mutex<()>,
    tx: broadcast::Sender<Arc<AgentMessage>>,
}

impl MessageBus {
    /// Open or lazily prepare the message bus at `<data_dir>/messages/log.jsonl` (or in-memory if `None`).
    /// Does NOT create any directory or file until the first message is sent.
    pub fn open(data_dir: Option<&Path>) -> Result<Arc<MessageBus>> {
        let path = data_dir.map(|d| d.join("messages").join("log.jsonl"));
        let mut store = Store { next_seq: 1, ..Default::default() };
        if let Some(p) = &path
            && p.is_file()
        {
            replay_journal(p, &mut store)?;
            if store.journal_lines > MAX_RETAINED.saturating_mul(2) {
                let _ = compact_journal(p, &store);
                store.journal_lines = store.messages.len();
            }
        }
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        Ok(Arc::new(MessageBus { path, store: RwLock::new(store), io_lock: Mutex::new(()), tx }))
    }

    pub fn in_memory() -> Arc<MessageBus> {
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        Arc::new(MessageBus {
            path: None,
            store: RwLock::new(Store { next_seq: 1, ..Default::default() }),
            io_lock: Mutex::new(()),
            tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AgentMessage>> {
        self.tx.subscribe()
    }

    /// Resolve `req.to` against live sessions, store the message, persist lazily, and push to subscribers.
    pub fn send(&self, engine: &Engine, req: SendMessageRequest) -> Result<SendMessageResponse, MessageError> {
        let text = req.text.trim();
        if text.is_empty() {
            return Err(MessageError::BadRequest("message text must not be empty".into()));
        }
        if req.text.len() > MAX_MESSAGE_BYTES {
            return Err(MessageError::BadRequest(format!("message text exceeds {} bytes limit", MAX_MESSAGE_BYTES)));
        }
        let exclude = req.from.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let target_sess = resolve_target_excluding(engine, &req.to, exclude)?;
        let from = exclude.unwrap_or("anonymous").to_owned();
        let kind = req.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let reply_to = req.reply_to.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let now = now_ms();

        let msg = self.store_message(from, target_sess.key.clone(), req.text, kind, reply_to, now)?;
        Ok(SendMessageResponse {
            message: msg,
            target: build_target(&target_sess),
            delivery_channel: None,
            delivery_detail: None,
            reply: None,
        })
    }

    /// Record a direct reply (e.g. captured from headless wake or live turn completion) onto the bus
    /// even when the recipient label is non-session (such as `"web-demo"` or `"cli"`).
    pub fn record_reply(
        &self,
        from: &str,
        to: &str,
        reply_to: &str,
        text: String,
    ) -> Result<AgentMessage, MessageError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(MessageError::BadRequest("reply text must not be empty".into()));
        }
        let clipped = if text.len() > MAX_MESSAGE_BYTES {
            let mut s = text;
            uniflo_schema::clip(&mut s, MAX_MESSAGE_BYTES);
            s
        } else {
            text
        };
        let to_clean = to.trim();
        let to_final = if to_clean.is_empty() { "anonymous" } else { to_clean };
        self.store_message(
            from.trim().to_owned(),
            to_final.to_owned(),
            clipped,
            Some("reply".into()),
            Some(reply_to.to_owned()),
            now_ms(),
        )
    }

    /// Snapshot of unread (`acked_at.is_none()`) message counts keyed by target session key.
    pub fn unread_counts(&self) -> HashMap<String, usize> {
        let st = self.store.read().unwrap();
        let mut counts = HashMap::new();
        for (target, seqs) in &st.by_target {
            let n = seqs.iter().filter_map(|seq| st.messages.get(seq)).filter(|m| m.acked_at.is_none()).count();
            if n > 0 {
                counts.insert(target.clone(), n);
            }
        }
        counts
    }

    fn store_message(
        &self,
        from: String,
        to: String,
        text: String,
        kind: Option<String>,
        reply_to: Option<String>,
        now: i64,
    ) -> Result<AgentMessage, MessageError> {
        let msg = {
            let _io = self.io_lock.lock().unwrap();
            let mut st = self.store.write().unwrap();
            let seq = st.next_seq;
            let msg = AgentMessage {
                id: format!("m-{seq}"),
                from,
                to,
                text,
                kind,
                reply_to,
                created_at: now,
                delivered_at: None,
                acked_at: None,
            };
            if let Some(p) = &self.path {
                append_entry(p, &JournalEntry::Send { seq, msg: msg.clone() })
                    .map_err(|e| MessageError::Internal(format!("persist message: {e:#}")))?;
                st.journal_lines += 1;
            }
            st.insert(seq, msg.clone());
            msg
        };
        let _ = self.tx.send(Arc::new(msg.clone()));
        Ok(msg)
    }

    /// Read messages matching `q` from the in-memory index (lock-free w.r.t. disk I/O).
    pub fn inbox(&self, engine: &Engine, q: &InboxQuery) -> Result<InboxResponse, MessageError> {
        let limit = if q.limit == 0 { 50 } else { q.limit.min(500) };
        let resolved_to = match q.to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => Some(resolve_inbox_target(engine, raw)?),
            None => None,
        };
        let resolved_peer = match q.peer.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => Some(resolve_inbox_target(engine, raw)?),
            None => None,
        };
        let cwd_keys: Option<Vec<String>> = q.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|dir| {
            let norm = normalize_dir(dir);
            let snap = engine.sessions_snapshot();
            snap.sorted
                .iter()
                .filter(|s| s.cwd.as_deref().is_some_and(|c| dir_matches(c, &norm)))
                .map(|s| s.key.clone())
                .collect()
        });
        let from_filter = q.from.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let st = self.store.read().unwrap();
        let mut matched: Vec<AgentMessage> = Vec::new();
        let mut unread = 0usize;

        let mut visit = |m: &AgentMessage| {
            if let Some(f) = from_filter
                && m.from != f
            {
                return;
            }
            if m.acked_at.is_none() {
                unread += 1;
            }
            if q.unread && m.acked_at.is_some() {
                return;
            }
            if q.undelivered && m.delivered_at.is_some() {
                return;
            }
            matched.push(m.clone());
        };

        if let Some(peer_key) = &resolved_peer {
            for m in st.messages.values() {
                if m.to == *peer_key || m.from == *peer_key {
                    visit(m);
                }
            }
        } else if let Some(key) = &resolved_to {
            if let Some(seqs) = st.by_target.get(key) {
                for &seq in seqs {
                    if let Some(m) = st.messages.get(&seq) {
                        visit(m);
                    }
                }
            }
        } else if let Some(keys) = &cwd_keys {
            let mut seqs: Vec<u64> =
                keys.iter().filter_map(|k| st.by_target.get(k)).flat_map(|v| v.iter().copied()).collect();
            seqs.sort_unstable();
            seqs.dedup();
            for seq in seqs {
                if let Some(m) = st.messages.get(&seq) {
                    visit(m);
                }
            }
        } else {
            for m in st.messages.values() {
                visit(m);
            }
        }

        // Return the newest `limit` messages in chronological order so agents read conversationally.
        if matched.len() > limit {
            matched.drain(..matched.len() - limit);
        }
        Ok(InboxResponse { to: resolved_to.or(resolved_peer), messages: matched, unread })
    }

    /// Mark specific message IDs or all pending messages for a target as delivered / acknowledged.
    pub fn ack(&self, engine: &Engine, req: AckMessagesRequest) -> Result<AckMessagesResponse, MessageError> {
        let mode = req.state.as_deref().unwrap_or("acked").trim();
        let now = now_ms();
        let (delivered_at, acked_at) = match mode {
            "delivered" => (Some(now), None),
            "acked" | "" => (Some(now), Some(now)),
            other => {
                return Err(MessageError::BadRequest(format!(
                    "invalid state {other:?}; expected \"delivered\" or \"acked\""
                )));
            }
        };

        let mut ids = req.ids;
        if let Some(raw_to) = req.to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let target = resolve_inbox_target(engine, raw_to)?;
            let st = self.store.read().unwrap();
            if let Some(seqs) = st.by_target.get(&target) {
                for &seq in seqs {
                    if let Some(m) = st.messages.get(&seq) {
                        let pending = if acked_at.is_some() { m.acked_at.is_none() } else { m.delivered_at.is_none() };
                        if pending {
                            ids.push(m.id.clone());
                        }
                    }
                }
            }
        }
        if ids.is_empty() {
            return Ok(AckMessagesResponse { updated: 0 });
        }
        ids.sort();
        ids.dedup();

        let updated = {
            let _io = self.io_lock.lock().unwrap();
            let mut st = self.store.write().unwrap();
            let n = st.apply_mark(&ids, delivered_at, acked_at);
            if n > 0
                && let Some(p) = &self.path
            {
                append_entry(p, &JournalEntry::Mark { ids, delivered_at, acked_at })
                    .map_err(|e| MessageError::Internal(format!("persist ack: {e:#}")))?;
                st.journal_lines += 1;
            }
            n
        };
        Ok(AckMessagesResponse { updated })
    }
}

/// Resolve a recipient specifier (`<harness>:<id>`, unique prefix, or `cwd:<dir>` / absolute path).
pub fn resolve_target(engine: &Engine, raw: &str) -> Result<Arc<Session>, MessageError> {
    resolve_target_excluding(engine, raw, None)
}

/// Like [`resolve_target`], but excludes `exclude` (typically the sender's own session key) when
/// resolving directory-based targets so two sessions sharing a `cwd` never loop messages back to
/// themselves.
pub fn resolve_target_excluding(
    engine: &Engine,
    raw: &str,
    exclude: Option<&str>,
) -> Result<Arc<Session>, MessageError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(MessageError::BadRequest("missing target `to`".into()));
    }
    let snap = engine.sessions_snapshot();
    if let Some(sess) = snap.by_key.get(raw) {
        return Ok(sess.clone());
    }
    if let Some(dir) =
        raw.strip_prefix("cwd:").or_else(|| (raw.starts_with('/') || raw.starts_with("~/")).then_some(raw))
    {
        let norm = normalize_dir(dir);
        if norm.is_empty() {
            return Err(MessageError::BadRequest("empty cwd target".into()));
        }
        let mut matches: Vec<&Arc<Session>> =
            snap.sorted.iter().filter(|s| s.cwd.as_deref().is_some_and(|c| dir_matches(c, &norm))).collect();
        if matches.is_empty() {
            return Err(MessageError::NotFound(format!("no session found in directory {dir}")));
        }
        if let Some(ex) = exclude {
            let others: Vec<&Arc<Session>> = matches.iter().copied().filter(|s| s.key != ex).collect();
            if !others.is_empty() {
                matches = others;
            }
        }
        // Prefer active Work -> idle with live pid -> newest updated_at (already sorted by updated_at desc).
        matches.sort_by_key(|s| match (s.status, s.pid.is_some()) {
            (Status::Work, _) => 0u8,
            (Status::Idle, true) => 1u8,
            (Status::Idle, false) => 2u8,
        });
        return Ok(matches[0].clone());
    }

    let hits: Vec<&Arc<Session>> =
        snap.sorted.iter().filter(|s| s.key.starts_with(raw) || s.id.starts_with(raw)).collect();
    match hits.as_slice() {
        [] => Err(MessageError::NotFound(format!("unknown target session {raw}"))),
        [one] => Ok((*one).clone()),
        many => {
            let cands: Vec<String> = many.iter().take(5).map(|s| s.key.clone()).collect();
            Err(MessageError::Ambiguous(format!("ambiguous target {raw:?}; matches: {}", cands.join(", ")), cands))
        }
    }
}

/// Resolve `to` for inbox/ack queries: accepts exact session keys even if the session exited/vanished
/// from the live index, otherwise resolves prefix or `cwd:` via live sessions.
fn resolve_inbox_target(engine: &Engine, raw: &str) -> Result<String, MessageError> {
    if raw.contains(':') && !raw.starts_with("cwd:") {
        let snap = engine.sessions_snapshot();
        if snap.by_key.contains_key(raw) {
            return Ok(raw.to_owned());
        }
        let hits: Vec<&Arc<Session>> = snap.sorted.iter().filter(|s| s.key.starts_with(raw)).collect();
        return match hits.as_slice() {
            [one] => Ok(one.key.clone()),
            [] => Ok(raw.to_owned()),
            many => {
                let cands: Vec<String> = many.iter().take(5).map(|s| s.key.clone()).collect();
                Err(MessageError::Ambiguous(format!("ambiguous target {raw:?}; matches: {}", cands.join(", ")), cands))
            }
        };
    }
    resolve_target(engine, raw).map(|s| s.key.clone())
}

fn build_target(sess: &Session) -> MessageTarget {
    let delivery = match (sess.status, sess.pid.is_some()) {
        (Status::Work, _) => "hook",
        (Status::Idle, true) => "pty_or_hook",
        (Status::Idle, false) => "resume",
    };
    let resume_command = resume::resume(sess).command;
    MessageTarget {
        key: sess.key.clone(),
        harness: sess.harness.clone(),
        title: sess.title.clone(),
        cwd: sess.cwd.clone(),
        status: sess.status,
        pid: sess.pid,
        delivery: delivery.to_owned(),
        resume_command,
    }
}

fn strip_macos_private(s: &str) -> &str {
    s.strip_prefix("/private")
        .filter(|r| *r == "/var" || *r == "/tmp" || r.starts_with("/var/") || r.starts_with("/tmp/"))
        .unwrap_or(s)
}

fn normalize_dir(raw: &str) -> String {
    let s = raw.trim();
    let expanded = if let Some(rest) = s.strip_prefix("~/") {
        format!("{}/{rest}", home().display())
    } else if s == "~" {
        home().display().to_string()
    } else {
        s.to_owned()
    };
    let norm = expanded.replace('\\', "/");
    let trimmed = norm.trim_end_matches('/');
    let out = if trimmed.is_empty() && norm.starts_with('/') { "/" } else { trimmed };
    strip_macos_private(out).to_owned()
}

pub fn dir_matches(session_cwd: &str, want_norm: &str) -> bool {
    let cur = session_cwd.replace('\\', "/");
    let cur = cur.trim_end_matches('/');
    let cur = if cur.is_empty() && session_cwd.starts_with('/') { "/" } else { cur };
    let cur = strip_macos_private(cur);
    let want = strip_macos_private(want_norm);
    cur == want || (want == "/" && cur.starts_with('/')) || cur.strip_prefix(want).is_some_and(|r| r.starts_with('/'))
}

fn replay_journal(path: &Path, store: &mut Store) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        store.journal_lines += 1;
        let Ok(entry) = serde_json::from_str::<JournalEntry>(line) else { continue };
        match entry {
            JournalEntry::Send { seq, msg } => store.insert(seq, msg),
            JournalEntry::Mark { ids, delivered_at, acked_at } => {
                store.apply_mark(&ids, delivered_at, acked_at);
            }
        }
    }
    Ok(())
}

fn append_entry(path: &Path, entry: &JournalEntry) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&line)?;
    Ok(())
}

fn compact_journal(path: &Path, store: &Store) -> Result<()> {
    let mut buf = Vec::new();
    for (&seq, msg) in &store.messages {
        serde_json::to_writer(&mut buf, &JournalEntry::Send { seq, msg: msg.clone() })?;
        buf.push(b'\n');
    }
    crate::pricing::sync::write_atomic(path, &buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HarnessInfo;
    use crate::adapter::{Adapter, Batch, Cursor, HistoryQuery, MetaPatch, ReadOutput, Record};
    use crate::engine::EngineOptions;
    use uniflo_schema::{Body, Event};

    struct FakeAdapter {
        sessions: Vec<(String, String, Status, Option<u32>, i64)>,
    }

    impl Adapter for FakeAdapter {
        fn info(&self) -> HarnessInfo {
            HarnessInfo { id: "claude", name: "Claude Code" }
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn source_for(&self, _: &Path) -> Option<PathBuf> {
            None
        }
        fn discover(&self) -> Vec<PathBuf> {
            self.sessions.iter().map(|(id, ..)| PathBuf::from(format!("/fake/{id}.jsonl"))).collect()
        }
        fn read(&self, src: &Path, _: Option<&Cursor>) -> Result<ReadOutput> {
            let stem = src.file_stem().unwrap().to_str().unwrap();
            let (id, cwd, status, _, ts) = self.sessions.iter().find(|(i, ..)| i == stem).unwrap();
            let mut items = vec![(
                id.clone(),
                Record::Meta(MetaPatch {
                    title: Some((2, format!("session {id}"))),
                    cwd: Some(cwd.clone()),
                    updated_at: Some(*ts),
                    ..Default::default()
                }),
            )];
            items.push((
                id.clone(),
                Record::Event(Event {
                    id: format!("e-{id}"),
                    session: format!("claude:{id}"),
                    ts: *ts,
                    pos: Some(0),
                    partial: false,
                    truncated: false,
                    body: if *status == Status::Work {
                        Body::TurnStart {}
                    } else {
                        Body::TurnEnd { reason: Some("done".into()) }
                    },
                }),
            ));
            Ok(ReadOutput {
                cursor: Cursor { offset: 1, size: 1, mtime_ms: *ts, state: serde_json::Value::Null },
                batch: Batch { items, bytes: 1, bad_lines: 0, unknown: Vec::new() },
                summary: false,
                reset: false,
            })
        }
        fn history(&self, _: &Path, _: &str, _: &HistoryQuery) -> Result<Vec<Event>> {
            Ok(Vec::new())
        }
    }

    fn test_engine() -> Arc<Engine> {
        let now = now_ms();
        let adapter: Arc<dyn Adapter> = Arc::new(FakeAdapter {
            sessions: vec![
                ("aaa111".into(), "/work/repo".into(), Status::Idle, None, now - 1_000),
                ("bbb222".into(), "/work/repo".into(), Status::Work, Some(1234), now - 5_000),
                ("ccc333".into(), "/work/other".into(), Status::Idle, None, now - 2_000),
            ],
        });
        let eng = Engine::new(vec![adapter], EngineOptions { cache_path: None, usage: false, ..Default::default() });
        eng.index();
        eng
    }

    #[test]
    fn lazy_persistence_send_deliver_ack_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let eng = test_engine();
        let bus = MessageBus::open(Some(dir.path())).unwrap();
        // Lazy: nothing created on disk before the first message.
        assert!(!dir.path().join("messages").exists());

        // Directory resolution prefers working session `bbb222` over newer idle `aaa111`.
        let sent = bus
            .send(
                &eng,
                SendMessageRequest {
                    to: "cwd:/work/repo".into(),
                    from: Some("claude:ccc333".into()),
                    text: "please rebase".into(),
                    kind: Some("task".into()),
                    reply_to: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(sent.target.key, "claude:bbb222");
        assert_eq!(sent.target.delivery, "hook");
        assert!(dir.path().join("messages/log.jsonl").is_file());

        // Undelivered inbox sees it.
        let inbox =
            bus.inbox(&eng, &InboxQuery { to: Some("bbb".into()), undelivered: true, ..Default::default() }).unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.unread, 1);

        // Hook marks delivered; undelivered becomes empty while unread remains 1.
        let d = bus
            .ack(
                &eng,
                AckMessagesRequest { ids: vec![sent.message.id.clone()], to: None, state: Some("delivered".into()) },
            )
            .unwrap();
        assert_eq!(d.updated, 1);
        assert!(
            bus.inbox(&eng, &InboxQuery { to: Some("claude:bbb222".into()), undelivered: true, ..Default::default() })
                .unwrap()
                .messages
                .is_empty()
        );
        assert_eq!(
            bus.inbox(&eng, &InboxQuery { to: Some("claude:bbb222".into()), unread: true, ..Default::default() })
                .unwrap()
                .messages
                .len(),
            1
        );

        // Explicit ack clears unread and persists across reopen.
        let a = bus
            .ack(&eng, AckMessagesRequest { ids: Vec::new(), to: Some("claude:bbb222".into()), state: None })
            .unwrap();
        assert_eq!(a.updated, 1);

        let reopened = MessageBus::open(Some(dir.path())).unwrap();
        let all = reopened.inbox(&eng, &InboxQuery { to: Some("claude:bbb222".into()), ..Default::default() }).unwrap();
        assert_eq!(all.messages.len(), 1);
        assert_eq!(all.unread, 0);
        assert!(all.messages[0].delivered_at.is_some());
        assert!(all.messages[0].acked_at.is_some());
    }

    #[test]
    fn validation_and_error_cases() {
        let eng = test_engine();
        let bus = MessageBus::in_memory();
        assert!(matches!(
            bus.send(&eng, SendMessageRequest { to: "claude:aaa111".into(), text: "   ".into(), ..Default::default() }),
            Err(MessageError::BadRequest(_))
        ));
        assert!(matches!(
            bus.send(
                &eng,
                SendMessageRequest {
                    to: "claude:aaa111".into(),
                    text: "x".repeat(MAX_MESSAGE_BYTES + 1),
                    ..Default::default()
                }
            ),
            Err(MessageError::BadRequest(_))
        ));
        assert!(matches!(
            bus.send(&eng, SendMessageRequest { to: "claude:".into(), text: "hi".into(), ..Default::default() }),
            Err(MessageError::Ambiguous(_, _))
        ));
        assert!(matches!(
            bus.send(&eng, SendMessageRequest { to: "claude:zzz".into(), text: "hi".into(), ..Default::default() }),
            Err(MessageError::NotFound(_))
        ));

        // Same-cwd routing excludes the sender (`claude:bbb222`, working) and routes to peer `claude:aaa111`.
        let routed = bus
            .send(
                &eng,
                SendMessageRequest {
                    to: "cwd:/work/repo".into(),
                    from: Some("claude:bbb222".into()),
                    text: "hey neighbor".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(routed.target.key, "claude:aaa111");

        // Two-way `peer` query sees both incoming and outgoing messages for `claude:aaa111`.
        let reply = bus.record_reply("claude:aaa111", "claude:bbb222", &routed.message.id, "pong".into()).unwrap();
        let conv = bus.inbox(&eng, &InboxQuery { peer: Some("claude:aaa111".into()), ..Default::default() }).unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].id, reply.id);
    }
}
