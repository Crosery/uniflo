//! OpenClaw (formerly Clawdbot) agent sessions. Entries are Pi session-tree entries and are
//! decoded by [`crate::pi::PiFamily`] (usage carries `usage.cost.total`).
//!
//! State directory: `$OPENCLAW_STATE_DIR`, `~/.openclaw`, then `~/.clawdbot` (the first with
//! `agents/`). Current builds keep one SQLite store per agent,
//! `agents/<agent>/agent/openclaw-agent.sqlite`: `session_windows` (session metadata,
//! `spawned_by` = the parent's session key), `transcript_events(session_id, seq, event_json)`
//! and `session_transcript_active_events(session_id, event_seq, active_position)`, the
//! visible branch of the tree. Growth of a visible branch is followed by `seq`; a branch
//! switch re-reads the store (`reset`). Without active rows every entry is shown in `seq`
//! order, as for Pi files. Older builds wrote `agents/<agent>/sessions/<id>.jsonl` with a
//! `sessions.json` index (labels, `spawnedBy`); a session present in the store is read
//! from the store only.

use crate::pi::PiFamily;
use crate::sqlite::{columns, int, nonempty, open_ro, text, wal_sig};
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, string_of, ts};
use uniflo_core::{
    Adapter, Cursor, Cx, HarnessInfo, HistoryQuery, JsonlAdapter, LineDecoder, MetaPatch, ReadOutput, Record, SourceId,
    decode_record,
};
use uniflo_schema::{Event, session_key};

const ID: &str = "openclaw";
const INFO: HarnessInfo = HarnessInfo { id: ID, name: "OpenClaw" };
const DB: &str = "openclaw-agent.sqlite";
/// Visible entries per session sampled by a summary read.
const WINDOW: usize = 40;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let h = home();
    let candidates: Vec<PathBuf> = std::env::var_os("OPENCLAW_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .chain([h.join(".openclaw"), h.join(".clawdbot")])
        .collect();
    let state = candidates.iter().find(|d| d.join("agents").is_dir()).unwrap_or(&candidates[0]);
    vec![Arc::new(OpenClaw::new(state.join("agents")))]
}

pub struct OpenClaw {
    root: PathBuf,
    legacy: JsonlAdapter<Legacy>,
}

/// Legacy `agents/<agent>/sessions/<id>.jsonl` transcripts.
pub struct Legacy {
    root: PathBuf,
    pi: PiFamily,
}

impl OpenClaw {
    fn new(root: PathBuf) -> Self {
        let pi = PiFamily::new(INFO, Vec::new(), None);
        OpenClaw { legacy: JsonlAdapter::new(Legacy { root: root.clone(), pi }), root }
    }

    fn is_db(&self, p: &Path) -> bool {
        p.file_name().is_some_and(|n| n == DB)
            && p.parent().and_then(|a| a.file_name()).is_some_and(|n| n == "agent")
            && p.ancestors().nth(3) == Some(self.root.as_path())
    }

    fn db_of_agent(&self, legacy: &Path) -> Option<PathBuf> {
        Some(legacy.parent()?.parent()?.join("agent").join(DB)).filter(|p| p.is_file())
    }
}

impl LineDecoder for Legacy {
    type State = ();

    fn info(&self) -> HarnessInfo {
        INFO
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// `<id>.jsonl` and `<id>-topic-<n>.jsonl`; `.deleted.` / `.reset.` / `.checkpoint.` copies are not sessions.
    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl")
            && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| !s.starts_with('.') && !s.contains('.'))
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == "sessions")
            && p.ancestors().nth(3) == Some(self.root.as_path())
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }

    fn max_depth(&self) -> usize {
        2
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        self.pi.decode(v, cx);
    }
}

/// Per-session follow state: newest `seq`, and the visible list (length + hash).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Seen {
    sig: (i64, i64, i64),
    len: usize,
    hash: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    sessions: BTreeMap<String, Seen>,
    wal_size: u64,
    wal_mtime: i64,
}

fn list_hash(seqs: &[i64]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    seqs.hash(&mut h);
    h.finish()
}

fn norm_ms(n: Option<i64>) -> Option<i64> {
    n.filter(|n| *n > 0).map(|n| if n > 100_000_000_000 { n } else { n * 1000 })
}

struct Window {
    id: String,
    patch: MetaPatch,
}

struct Store {
    c: Connection,
    active: bool,
}

impl Store {
    fn open(src: &Path) -> Result<Store> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let active = !columns(&c, "session_transcript_active_events")?.is_empty();
        Ok(Store { c, active })
    }

    /// Sessions that have entries, with metadata; `spawned_by` keys resolve to session ids.
    fn windows(&self) -> Result<Vec<Window>> {
        let w = columns(&self.c, "session_windows")?;
        let col = |n: &str| if w.contains(n) { format!("w.{n}") } else { "NULL".to_owned() };
        let labels = !columns(&self.c, "session_nodes")?.is_empty();
        let label = if labels {
            "(SELECT COALESCE(NULLIF(n.label,''), n.display_name) FROM session_nodes n WHERE n.session_key = w.session_key)"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT w.session_id, {}, {}, {}, {}, {}, {}, {}, {}, {label} FROM session_windows w
             WHERE EXISTS (SELECT 1 FROM transcript_events e WHERE e.session_id = w.session_id)",
            col("session_key"),
            col("spawned_by"),
            col("model"),
            col("display_name"),
            col("started_at"),
            col("created_at"),
            col("updated_at"),
            col("transcript_updated_at"),
        );
        let mut st = self.c.prepare(&sql)?;
        let rows: Vec<(String, Option<String>, Option<String>, MetaPatch)> = st
            .query_map([], |r| {
                let created = norm_ms(int(r, 5)).or_else(|| norm_ms(int(r, 6)));
                let updated = [norm_ms(int(r, 7)), norm_ms(int(r, 8))].into_iter().flatten().max();
                let patch = MetaPatch {
                    title: nonempty(r, 9).or_else(|| nonempty(r, 4)).map(|t| (2, t)),
                    model: nonempty(r, 3),
                    started_at: created,
                    updated_at: updated,
                    ..Default::default()
                };
                Ok((text(r, 0).unwrap_or_default(), nonempty(r, 1), nonempty(r, 2), patch))
            })?
            .flatten()
            .collect();
        let by_key: HashMap<&str, &str> =
            rows.iter().filter_map(|(id, k, _, _)| Some((k.as_deref()?, id.as_str()))).collect();
        Ok(rows
            .iter()
            .map(|(id, _, spawned, patch)| {
                let parent = spawned.as_deref().map(|s| by_key.get(s).copied().unwrap_or(s).to_owned());
                Window { id: id.clone(), patch: MetaPatch { parent: parent.filter(|p| p != id), ..patch.clone() } }
            })
            .collect())
    }

    /// Change signature per session: (newest seq, active rows, Σ active seqs).
    fn sigs(&self) -> Result<HashMap<String, (i64, i64, i64)>> {
        let mut out: HashMap<String, (i64, i64, i64)> = HashMap::new();
        let mut st = self.c.prepare("SELECT session_id, MAX(seq) FROM transcript_events GROUP BY session_id")?;
        for (id, max) in st.query_map([], |r| Ok((text(r, 0).unwrap_or_default(), int(r, 1).unwrap_or(0))))?.flatten() {
            out.entry(id).or_default().0 = max;
        }
        if self.active {
            let mut st = self.c.prepare(
                "SELECT session_id, COUNT(*), TOTAL(event_seq) FROM session_transcript_active_events GROUP BY session_id",
            )?;
            for row in st.query_map([], |r| {
                Ok((text(r, 0).unwrap_or_default(), int(r, 1).unwrap_or(0), r.get::<_, f64>(2).unwrap_or(0.0) as i64))
            })? {
                let (id, n, sum) = row?;
                let e = out.entry(id).or_default();
                (e.1, e.2) = (n, sum);
            }
        }
        Ok(out)
    }

    /// Visible entry seqs: the active branch (header first), or every entry.
    fn visible(&self, sid: &str) -> Result<Vec<i64>> {
        let all = |sql: &str| -> Result<Vec<i64>> {
            let mut st = self.c.prepare_cached(sql)?;
            let v = st.query_map([sid], |r| r.get::<_, i64>(0))?.flatten().collect();
            Ok(v)
        };
        let mut list = if self.active {
            all("SELECT event_seq FROM session_transcript_active_events WHERE session_id=?1 ORDER BY active_position")?
        } else {
            Vec::new()
        };
        if list.is_empty() {
            return all("SELECT seq FROM transcript_events WHERE session_id=?1 ORDER BY seq");
        }
        let first = all("SELECT MIN(seq) FROM transcript_events WHERE session_id=?1")?;
        if let Some(&f) = first.first()
            && !list.contains(&f)
        {
            list.insert(0, f);
        }
        Ok(list)
    }

    fn decode(&self, pi: &PiFamily, src: &Path, sid: &str, seqs: &[i64], batch: &mut Batch) -> Result<()> {
        let key = session_key(ID, sid);
        let mut st =
            self.c.prepare_cached("SELECT event_json FROM transcript_events WHERE session_id=?1 AND seq=?2")?;
        for &seq in seqs {
            let Some(raw) = st.query_row(rusqlite::params![sid, seq], |r| Ok(text(r, 0))).ok().flatten() else {
                continue;
            };
            match serde_json::from_str::<Value>(&raw) {
                Ok(v) => {
                    let d = decode_record(pi, &key, src, seq.max(0) as u64, &v, &mut ());
                    batch.items.extend(d.records.into_iter().map(|r| (sid.to_owned(), r)));
                    batch.unknown.extend(d.unknown);
                }
                Err(_) => batch.bad_lines += 1,
            }
        }
        Ok(())
    }
}

impl OpenClaw {
    fn pi(&self) -> &PiFamily {
        &self.legacy.decoder.pi
    }

    fn cursor_for(src: &Path, st: State) -> Cursor {
        let md = std::fs::metadata(src).ok();
        Cursor {
            offset: 0,
            size: md.as_ref().map_or(0, |m| m.len()),
            mtime_ms: md.as_ref().map_or(0, file_mtime_ms),
            state: serde_json::to_value(st).unwrap_or_default(),
        }
    }

    fn read_db(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let s = Store::open(src)?;
        let windows = s.windows()?;
        let sigs = s.sigs()?;
        let (wal_size, wal_mtime) = wal_sig(src);
        let prev = cursor.and_then(|c| serde_json::from_value::<State>(c.state.clone()).ok());
        let mut st = State { wal_size, wal_mtime, ..Default::default() };
        let mut batch = Batch::default();
        let mut reset = false;
        for w in &windows {
            let sig = sigs.get(&w.id).copied().unwrap_or_default();
            let old = prev.as_ref().and_then(|p| p.sessions.get(&w.id));
            if let Some(o) = old.filter(|o| o.sig == sig) {
                st.sessions.insert(w.id.clone(), o.clone());
                continue;
            }
            let vis = s.visible(&w.id)?;
            let take = match (prev.as_ref(), old) {
                // Same branch, grown: only the new tail.
                (Some(_), Some(o)) if vis.len() >= o.len && list_hash(&vis[..o.len]) == o.hash => &vis[o.len..],
                (Some(_), Some(_)) => {
                    reset = true;
                    break;
                }
                (Some(_), None) => &vis[..],
                (None, _) => &vis[vis.len().saturating_sub(WINDOW)..],
            };
            batch.items.push((w.id.clone(), Record::Meta(w.patch.clone())));
            if cursor.is_none() && take.len() < vis.len() {
                // The header entry carries the cwd.
                s.decode(self.pi(), src, &w.id, &vis[..1], &mut batch)?;
            }
            s.decode(self.pi(), src, &w.id, take, &mut batch)?;
            st.sessions.insert(w.id.clone(), Seen { sig, len: vis.len(), hash: list_hash(&vis) });
        }
        if reset {
            return self.read_db(src, None).map(|o| ReadOutput { reset: true, ..o });
        }
        Ok(ReadOutput { cursor: Self::cursor_for(src, st), batch, summary: cursor.is_none(), reset: false })
    }

    /// `sessions.json` of a legacy transcript: label, model, parent.
    fn legacy_meta(&self, p: &Path, id: &str) -> Option<MetaPatch> {
        let idx: Value = serde_json::from_slice(&std::fs::read(p.parent()?.join("sessions.json")).ok()?).ok()?;
        let obj = idx.as_object()?;
        let base = id.split("-topic-").next().unwrap_or(id);
        let entry = obj.values().find(|v| v.get("sessionId").and_then(Value::as_str) == Some(base))?;
        let parent =
            string_of(entry, "spawnedBy").map(|k| obj.get(&k).and_then(|e| string_of(e, "sessionId")).unwrap_or(k));
        Some(MetaPatch {
            parent: parent.filter(|p| p != id),
            title: string_of(entry, "label").or_else(|| string_of(entry, "displayName")).map(|t| (2, t)),
            model: string_of(entry, "model"),
            updated_at: entry.get("updatedAt").and_then(ts),
            ..Default::default()
        })
    }

    /// Whether the agent's store already holds this legacy session.
    fn in_store(&self, p: &Path, id: &str) -> bool {
        let Some(db) = self.db_of_agent(p) else { return false };
        let Ok(c) = open_ro(&db) else { return false };
        let base = id.split("-topic-").next().unwrap_or(id);
        c.query_row("SELECT 1 FROM transcript_events WHERE session_id=?1 LIMIT 1", [base], |_| Ok(())).is_ok()
    }
}

impl Adapter for OpenClaw {
    fn info(&self) -> HarnessInfo {
        INFO
    }

    fn roots(&self) -> Vec<PathBuf> {
        if self.root.is_dir() { vec![self.root.clone()] } else { Vec::new() }
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        let name = path.file_name()?.to_str()?;
        if name.starts_with(DB) {
            let db = path.with_file_name(DB);
            return (self.is_db(&db) && crate::sqlite::source_for_db(&db, path).is_some()).then_some(db);
        }
        self.legacy.source_for(path)
    }

    fn discover(&self) -> Vec<PathBuf> {
        let mut out = self.legacy.discover();
        if let Ok(rd) = std::fs::read_dir(&self.root) {
            out.extend(rd.flatten().map(|e| e.path().join("agent").join(DB)).filter(|p| p.is_file()));
        }
        out
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        if self.is_db(src) {
            return self.read_db(src, cursor);
        }
        let id = self.legacy.decoder.identify(src).map(|s| s.id).unwrap_or_default();
        let mut out = self.legacy.read(src, cursor)?;
        if self.in_store(src, &id) {
            out.batch.items.clear();
            out.batch.unknown.clear();
        } else if let Some(m) = self.legacy_meta(src, &id).filter(|_| cursor.is_none()) {
            out.batch.items.insert(0, (id, Record::Meta(m)));
        }
        Ok(out)
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        if !self.is_db(src) {
            return self.legacy.history(src, session_id, q);
        }
        let s = Store::open(src)?;
        let before = q.before.map_or(i64::MAX, |b| b.min(i64::MAX as u64) as i64);
        let vis: Vec<i64> = s.visible(session_id)?.into_iter().filter(|q| *q < before).collect();
        let mut batch = Batch::default();
        s.decode(self.pi(), src, session_id, &vis[vis.len().saturating_sub(q.limit.max(1))..], &mut batch)?;
        Ok(batch
            .items
            .into_iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                Record::Meta(_) => None,
            })
            .collect())
    }

    fn read_all(&self, src: &Path, ids: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        if !self.is_db(src) {
            let id = self.legacy.decoder.identify(src).map(|s| s.id).unwrap_or_default();
            if self.in_store(src, &id) {
                return Ok(self.legacy.read(src, None)?.cursor);
            }
            return self.legacy.read_all(src, ids, sink);
        }
        let s = Store::open(src)?;
        let (wal_size, wal_mtime) = wal_sig(src);
        let sigs = s.sigs()?;
        let mut st = State { wal_size, wal_mtime, ..Default::default() };
        for w in s.windows()? {
            let vis = s.visible(&w.id)?;
            if ids.contains(&w.id) {
                let mut batch = Batch::default();
                batch.items.push((w.id.clone(), Record::Meta(w.patch.clone())));
                s.decode(self.pi(), src, &w.id, &vis, &mut batch)?;
                for (sid, r) in batch.items {
                    sink(&sid, r);
                }
            }
            let sig = sigs.get(&w.id).copied().unwrap_or_default();
            st.sessions.insert(w.id, Seen { sig, len: vis.len(), hash: list_hash(&vis) });
        }
        Ok(Self::cursor_for(src, st))
    }

    fn changed(&self, src: &Path, cursor: &Cursor) -> bool {
        if !self.is_db(src) {
            return self.legacy.changed(src, cursor);
        }
        let Ok(md) = std::fs::metadata(src) else { return true };
        if md.len() != cursor.size || file_mtime_ms(&md) != cursor.mtime_ms {
            return true;
        }
        let (size, mtime) = wal_sig(src);
        serde_json::from_value::<State>(cursor.state.clone())
            .map_or(true, |st| st.wal_size != size || st.wal_mtime != mtime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, group, kinds};
    use serde_json::json;
    use uniflo_schema::{Body, Status};

    const SCHEMA: &str = "
        CREATE TABLE session_windows (session_id TEXT PRIMARY KEY, session_key TEXT, spawned_by TEXT, created_at INTEGER,
            updated_at INTEGER, transcript_updated_at INTEGER, started_at INTEGER, model TEXT, channel TEXT, display_name TEXT);
        CREATE TABLE session_nodes (session_key TEXT PRIMARY KEY, label TEXT, display_name TEXT, entry_json TEXT);
        CREATE TABLE transcript_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER,
            PRIMARY KEY (session_id, seq));
        CREATE TABLE session_transcript_active_events (session_id TEXT, event_seq INTEGER, active_position INTEGER);";

    fn msg(id: &str, parent: Option<&str>, m: Value) -> Value {
        json!({"type":"message","id":id,"parentId":parent,"timestamp":"2026-09-10T10:00:00Z","message":m})
    }

    fn put(c: &Connection, sid: &str, seq: i64, v: Value) {
        c.execute(
            "INSERT INTO transcript_events VALUES (?1, ?2, ?3, 1790000000)",
            rusqlite::params![sid, seq, v.to_string()],
        )
        .unwrap();
    }

    fn activate(c: &Connection, sid: &str, seqs: &[i64]) {
        c.execute("DELETE FROM session_transcript_active_events WHERE session_id=?1", [sid]).unwrap();
        for (i, s) in seqs.iter().enumerate() {
            c.execute(
                "INSERT INTO session_transcript_active_events VALUES (?1, ?2, ?3)",
                rusqlite::params![sid, s, i as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn store_branch_wins_over_legacy_copy_with_cost_and_parent() {
        let fx = Fixture::new();
        let root = fx.root().join("agents");
        let db = root.join("main/agent").join(DB);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = Connection::open(&db).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute(
            "INSERT INTO session_windows VALUES ('s1','agent:main:main',NULL,1790000000000,1790000100000,1790000100000,
             1790000000000,'claude-opus-4.1','cli',NULL),
             ('s2','agent:main:sub:1','agent:main:main',1790000050,1790000060,NULL,NULL,'claude-haiku',NULL,'helper')",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO session_nodes VALUES ('agent:main:main','Ship it',NULL,'{}')", []).unwrap();
        let usage = json!({"input":20,"output":8,"cacheRead":100,"cacheWrite":0,"cost":{"total":0.0042}});
        put(
            &c,
            "s1",
            1,
            json!({"type":"session","version":3,"id":"s1","timestamp":"2026-09-21T14:13:20Z","cwd":"/w/claw"}),
        );
        put(&c, "s1", 2, msg("u1", None, json!({"role":"user","content":[{"type":"text","text":"deploy"}]})));
        put(
            &c,
            "s1",
            3,
            msg(
                "a1",
                Some("u1"),
                json!({"role":"assistant","content":[{"type":"text","text":"old branch"}],"stopReason":"stop"}),
            ),
        );
        put(
            &c,
            "s1",
            4,
            msg(
                "a2",
                Some("u1"),
                json!({"role":"assistant","content":[{"type":"text","text":"deployed"}],
            "model":"claude-opus-4.1","usage":usage,"stopReason":"stop"}),
            ),
        );
        activate(&c, "s1", &[2, 4]);
        put(&c, "s2", 1, msg("x1", None, json!({"role":"user","content":[{"type":"text","text":"sub task"}]})));
        // The legacy copy of s1 (pre-migration) must not produce a second session.
        let legacy = format!("{}\n", json!({"type":"session","id":"s1","cwd":"/old"}))
            + &format!("{}\n", msg("u1", None, json!({"role":"user","content":[{"type":"text","text":"legacy"}]})));
        let lp = fx.write("agents/main/sessions/s1.jsonl", &legacy);
        let a = OpenClaw::new(root.clone());
        let mut found = a.discover();
        found.sort();
        assert_eq!(found, vec![db.clone(), lp.clone()]);
        assert!(a.read(&lp, None).unwrap().batch.items.is_empty(), "the store wins");

        let out = a.read(&db, None).unwrap();
        let g = group(out.batch);
        assert_eq!(g.keys().collect::<Vec<_>>(), ["s1", "s2"]);
        let r = &g["s1"];
        assert_eq!(kinds(&r.events), ["user_message", "assistant_message", "usage", "turn_end"]);
        assert!(matches!(&r.events[1].body, Body::AssistantMessage { text, .. } if text == "deployed"));
        assert!(matches!(&r.events[2].body, Body::Usage(u)
            if u.cost_usd == Some(0.0042) && (u.input, u.cache_read) == (20, 100) && u.model.as_deref() == Some("claude-opus-4.1")));
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/claw"));
        assert_eq!(r.meta.title.as_deref(), Some("Ship it"));
        assert_eq!(r.meta.model.as_deref(), Some("claude-opus-4.1"));
        assert_eq!(r.meta.started_at, Some(1790000000000));
        assert!(r.meta.parent.is_none());
        assert_eq!(g["s2"].meta.parent.as_deref(), Some("s1"), "spawned_by resolves the parent key");
        assert_eq!(g["s2"].meta.started_at, Some(1790000050000), "seconds normalized");
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);

        // The branch grows: only the new entry. Then it switches: the store is re-read.
        put(&c, "s1", 5, msg("u2", Some("a2"), json!({"role":"user","content":[{"type":"text","text":"and docs"}]})));
        activate(&c, "s1", &[2, 4, 5]);
        let out2 = a.read(&db, Some(&out.cursor)).unwrap();
        assert!(!out2.reset);
        let g2 = group(out2.batch);
        assert_eq!(kinds(&g2["s1"].events), ["user_message"]);
        assert!(!g2.contains_key("s2"), "unchanged sessions are skipped");
        activate(&c, "s1", &[2, 3]);
        let out3 = a.read(&db, Some(&out2.cursor)).unwrap();
        assert!(out3.reset);
        let g3 = group(out3.batch);
        assert!(matches!(&g3["s1"].events[1].body, Body::AssistantMessage { text, .. } if text == "old branch"));
        let h = a.history(&db, "s1", &HistoryQuery { before: None, limit: 10 }).unwrap();
        assert_eq!(kinds(&h), ["user_message", "assistant_message", "turn_end"]);
    }

    #[test]
    fn legacy_jsonl_with_index_metadata() {
        let fx = Fixture::new();
        let root = fx.root().join("agents");
        let s = format!("{}\n", json!({"type":"session","id":"L1","timestamp":"2026-09-10T10:00:00Z","cwd":"/w/l"}))
            + &format!("{}\n", msg("u1", None, json!({"role":"user","content":[{"type":"text","text":"hi"}]})))
            + &format!(
                "{}\n",
                msg(
                    "a1",
                    Some("u1"),
                    json!({"role":"assistant","content":[{"type":"text","text":"yo"}],"stopReason":"stop"})
                )
            );
        let p = fx.write("agents/ops/sessions/L1.jsonl", &s);
        fx.write("agents/ops/sessions/L1.deleted.2026.jsonl", &s);
        fx.write(
            "agents/ops/sessions/sessions.json",
            &json!({"agent:ops:main":{"sessionId":"P0","label":"Main"},
                    "agent:ops:sub":{"sessionId":"L1","label":"Ops chat","model":"gpt-5","spawnedBy":"agent:ops:main"}})
            .to_string(),
        );
        let a = OpenClaw::new(root);
        assert_eq!(a.discover(), vec![p.clone()]);
        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events), ["user_message", "assistant_message", "turn_end"]);
        assert_eq!(r.meta.title.as_deref(), Some("Ops chat"));
        assert_eq!(r.meta.parent.as_deref(), Some("P0"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/l"));
        assert_eq!(r.status(), Status::Idle);
    }
}
