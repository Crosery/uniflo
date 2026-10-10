//! Cursor IDE's global store `state.vscdb` (`cursorDiskKV`, `composerHeaders`), read for the
//! `cursor` harness: metadata of agent-transcript sessions, and the conversations of older
//! IDE composers that kept their messages ("bubbles") in the store.
//!
//! Keys: `composerData:<composerId>` (name, `createdAt` / `lastUpdatedAt` ms,
//! `modelConfig.modelName`, `workspaceIdentifier.uri.fsPath`, `trackedGitRepos[].repoPath`,
//! `fullConversationHeadersOnly[]{bubbleId, type}` — the only message order) and
//! `bubbleId:<composerId>:<bubbleId>` (`type` 1 user / 2 assistant, `text`, `thinking.text`,
//! `toolFormerData`, `tokenCount`, `modelInfo.modelName`). `composerHeaders` rows with
//! `isSubagent = 1` name the parent (`subagentInfo.parentComposerId`). The store can be
//! gigabytes: enumeration only walks key ranges (never `json_extract` over every blob), single
//! values are read by key, and nothing is re-read until the file or its WAL changes.

use crate::sqlite::{columns, open_ro, text, wal_sig};
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, ts};
use uniflo_core::{Cursor, MetaPatch, Record};
use uniflo_schema::{Body, Event, Usage, session_key};

const BUBBLE: &str = "bubbleId:";
const DATA: &str = "composerData:";

/// `state.vscdb` of the Cursor IDE on this OS.
pub fn default_db() -> PathBuf {
    let h = home();
    let base = if cfg!(target_os = "macos") {
        h.join("Library/Application Support/Cursor")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map_or_else(|| h.join("AppData/Roaming"), PathBuf::from).join("Cursor")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| h.join(".config"), PathBuf::from)
            .join("Cursor")
    };
    base.join("User/globalStorage/state.vscdb")
}

/// (db size, db mtime, wal size, wal mtime): the store is re-read only when this moves.
pub type Sig = (u64, i64, u64, i64);

pub fn sig(db: &Path) -> Sig {
    let (len, mtime) = std::fs::metadata(db).map_or((0, 0), |m| (m.len(), file_mtime_ms(&m)));
    let (ws, wm) = wal_sig(db);
    (len, mtime, ws, wm)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    sig: Sig,
}

pub struct Ide {
    pub db: PathBuf,
    cache: Mutex<Option<Cached>>,
}

/// What was read from one version of the store (its [`Sig`]).
struct Cached {
    sig: Sig,
    /// Composer metadata by id (`None`: no `composerData` row).
    metas: HashMap<String, Option<MetaPatch>>,
    /// Sub-agent → parent composer, read on first use.
    parents: Option<HashMap<String, String>>,
}

fn data_meta(d: &Value) -> MetaPatch {
    let cwd = d
        .pointer("/workspaceIdentifier/uri/fsPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| d.pointer("/trackedGitRepos/0/repoPath").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(str::to_owned);
    MetaPatch {
        title: string_of(d, "name").map(|t| (2, t)),
        cwd,
        model: d
            .pointer("/modelConfig/modelName")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty() && *m != "default")
            .map(str::to_owned),
        started_at: d.get("createdAt").and_then(ts),
        updated_at: d.get("lastUpdatedAt").and_then(ts),
        ..Default::default()
    }
}

impl Ide {
    pub fn new(db: PathBuf) -> Self {
        Ide { db, cache: Mutex::new(None) }
    }

    fn value(c: &Connection, key: &str) -> Option<Value> {
        let raw = c.query_row("SELECT value FROM cursorDiskKV WHERE key = ?1", [key], |r| Ok(text(r, 0))).ok()??;
        serde_json::from_str(&raw).ok()
    }

    fn parents(c: &Connection) -> HashMap<String, String> {
        if columns(c, "composerHeaders").map_or(true, |cols| !cols.contains("isSubagent")) {
            return HashMap::new();
        }
        let Ok(mut st) = c.prepare("SELECT composerId, value FROM composerHeaders WHERE isSubagent = 1") else {
            return HashMap::new();
        };
        st.query_map([], |r| Ok((text(r, 0), text(r, 1))))
            .map(|rows| {
                rows.flatten()
                    .filter_map(|(id, v)| {
                        let v: Value = serde_json::from_str(&v?).ok()?;
                        let p = v.pointer("/subagentInfo/parentComposerId").and_then(Value::as_str)?.to_owned();
                        Some((id?, p))
                    })
                    .filter(|(id, p)| !p.is_empty() && id != p)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// IDE metadata of composer `id` (title, cwd, model, times, parent), when the store has it.
    pub fn meta(&self, id: &str) -> Option<MetaPatch> {
        if !self.db.is_file() {
            return None;
        }
        let now = sig(&self.db);
        let mut g = self.cache.lock().unwrap();
        if g.as_ref().is_none_or(|c| c.sig != now) {
            *g = Some(Cached { sig: now, metas: HashMap::new(), parents: None });
        }
        let cached = g.as_mut()?;
        if let Some(m) = cached.metas.get(id) {
            return m.clone();
        }
        let c = open_ro(&self.db).ok()?;
        let parents = cached.parents.get_or_insert_with(|| Self::parents(&c));
        let m = Self::value(&c, &format!("{DATA}{id}"))
            .map(|d| MetaPatch { parent: parents.get(id).cloned(), ..data_meta(&d) });
        cached.metas.insert(id.to_owned(), m.clone());
        m
    }

    /// Composers that have at least one bubble row, by skipping from composer to composer
    /// over the key index (no values read).
    fn with_bubbles(c: &Connection) -> Result<Vec<String>> {
        let mut st =
            c.prepare("SELECT key FROM cursorDiskKV WHERE key > ?1 AND key < 'bubbleId;' ORDER BY key LIMIT 1")?;
        let mut out = Vec::new();
        let mut from = BUBBLE.to_owned();
        while let Ok(key) = st.query_row([&from], |r| r.get::<_, String>(0)) {
            let Some(cid) = key[BUBBLE.len()..].split(':').next().filter(|s| !s.is_empty()) else { break };
            out.push(cid.to_owned());
            from = format!("{BUBBLE}{cid};");
        }
        Ok(out)
    }

    /// The conversation of one composer in `fullConversationHeadersOnly` order.
    fn conversation(c: &Connection, cid: &str, parents: &HashMap<String, String>, batch: &mut Batch) -> Result<bool> {
        let Some(data) = Self::value(c, &format!("{DATA}{cid}")) else { return Ok(false) };
        let prefix = format!("{BUBBLE}{cid}:");
        let mut st = c.prepare_cached("SELECT substr(key, ?2), value FROM cursorDiskKV WHERE key > ?1 AND key < ?3")?;
        let end = format!("{BUBBLE}{cid};");
        let bubbles: HashMap<String, Value> = st
            .query_map(rusqlite::params![prefix, prefix.len() as i64 + 1, end], |r| Ok((text(r, 0), text(r, 1))))?
            .flatten()
            .filter_map(|(k, v)| Some((k?, serde_json::from_str(&v?).ok()?)))
            .collect();
        let heads = data.get("fullConversationHeadersOnly").and_then(Value::as_array).cloned().unwrap_or_default();
        let ordered: Vec<(u8, &Value, &str)> = heads
            .iter()
            .filter_map(|h| {
                let bid = str_of(h, "bubbleId")?;
                let b = bubbles.get(bid)?;
                let kind = h.get("type").or_else(|| b.get("type")).and_then(Value::as_u64).unwrap_or(2) as u8;
                Some((kind, b, bid))
            })
            .collect();
        let meta = MetaPatch { parent: parents.get(cid).cloned(), ..data_meta(&data) };
        let model = meta.model.clone();
        let key = session_key("cursor", cid);
        let mut evs: Vec<(String, i64, u64, Body)> = Vec::new();
        for (i, (kind, b, bid)) in ordered.iter().enumerate() {
            let pos = i as u64 + 1;
            let t = b.get("createdAt").and_then(ts).unwrap_or(0);
            let text = str_of(b, "text").unwrap_or("").to_owned();
            if *kind == 1 {
                if !text.trim().is_empty() {
                    evs.push((format!("b{bid}"), t, pos, Body::UserMessage { text, synthetic: false }));
                }
                continue;
            }
            let bmodel = b
                .pointer("/modelInfo/modelName")
                .and_then(Value::as_str)
                .filter(|m| *m != "default")
                .map(str::to_owned);
            if let Some(th) = b.pointer("/thinking/text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
                evs.push((format!("b{bid}:r"), t, pos, Body::Reasoning { text: th.to_owned() }));
            }
            if !text.trim().is_empty() {
                let m = bmodel.clone().or_else(|| model.clone());
                evs.push((format!("b{bid}"), t, pos, Body::AssistantMessage { text, model: m }));
            }
            let tool = b.get("toolFormerData").filter(|d| d.is_object());
            if let Some(d) = tool {
                let call_id = string_of(d, "toolCallId").unwrap_or_else(|| format!("b{bid}"));
                let name = string_of(d, "name").unwrap_or_else(|| "tool".into());
                let input = d.get("rawArgs").or_else(|| d.get("params")).map(json_arg).unwrap_or(Value::Null);
                evs.push((
                    format!("b{bid}:c"),
                    t,
                    pos,
                    Body::ToolCall { call_id: call_id.clone(), name: name.clone(), input },
                ));
                if let Some(r) = d.get("result").filter(|r| !r.is_null()) {
                    let r = json_arg(r);
                    let output = r
                        .get("output")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| r.as_str().map_or_else(|| r.to_string(), str::to_owned));
                    let is_error = str_of(d, "status") == Some("error");
                    evs.push((
                        format!("b{bid}:o"),
                        t,
                        pos,
                        Body::ToolResult { call_id, name: Some(name), output, is_error },
                    ));
                }
            }
            if let Some(tc) = b.get("tokenCount") {
                let n = |k: &str| tc.get(k).and_then(Value::as_u64).unwrap_or(0);
                let usage = Usage {
                    input: n("inputTokens"),
                    output: n("outputTokens"),
                    model: bmodel.or_else(|| model.clone()),
                    ..Default::default()
                };
                if usage.input + usage.output > 0 {
                    evs.push((format!("b{bid}:u"), t, pos, Body::Usage(usage)));
                }
            }
            let next_user = ordered.get(i + 1).is_none_or(|(k, _, _)| *k == 1);
            if tool.is_none() && next_user && !str_of(b, "text").unwrap_or("").trim().is_empty() {
                evs.push((format!("b{bid}:end"), t, pos, Body::TurnEnd { reason: Some("stop".into()) }));
            }
        }
        if evs.is_empty() {
            return Ok(false);
        }
        batch.items.push((cid.to_owned(), Record::Meta(meta)));
        for (id, t, pos, body) in evs {
            let e = Event { id, session: key.clone(), ts: t, pos: Some(pos), partial: false, truncated: false, body };
            batch.items.push((cid.to_owned(), Record::Event(e)));
        }
        Ok(true)
    }

    /// IDE-only composers with a conversation (ids in `skip` have an agent transcript), or
    /// just composer `only`.
    pub fn read(&self, skip: &HashSet<String>, only: Option<&str>) -> Result<(Batch, Cursor)> {
        let now = sig(&self.db);
        let c = open_ro(&self.db)?;
        c.execute_batch("BEGIN")?;
        let parents = Self::parents(&c);
        let mut batch = Batch::default();
        for cid in Self::with_bubbles(&c)? {
            if skip.contains(&cid) || only.is_some_and(|o| o != cid) {
                continue;
            }
            Self::conversation(&c, &cid, &parents, &mut batch)?;
        }
        let cursor =
            Cursor { offset: 0, size: now.0, mtime_ms: now.1, state: serde_json::to_value(State { sig: now })? };
        Ok((batch, cursor))
    }

    pub fn changed(&self, cursor: &Cursor) -> bool {
        serde_json::from_value::<State>(cursor.state.clone()).map_or(true, |s| s.sig != sig(&self.db))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    /// A synthetic store: `kv` rows plus `composerHeaders` (`id`, `isSubagent`, `value`).
    pub fn store(db: &Path, kv: &[(String, Value)], headers: &[(&str, i64, Value)]) {
        let w = Connection::open(db).unwrap();
        w.execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER,
                lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER,
                value TEXT, subagentTypeName TEXT);",
        )
        .unwrap();
        for (k, v) in kv {
            w.execute("INSERT INTO cursorDiskKV VALUES (?1, ?2)", rusqlite::params![k, v.to_string()]).unwrap();
        }
        for (id, sub, v) in headers {
            w.execute(
                "INSERT INTO composerHeaders(composerId, isSubagent, value) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, sub, v.to_string()],
            )
            .unwrap();
        }
    }

    #[test]
    fn skip_scan_finds_each_composer_with_bubbles_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.vscdb");
        let b = |c: &str, id: &str| (format!("{BUBBLE}{c}:{id}"), json!({"type":1,"text":"x"}));
        store(&db, &[b("aa", "1"), b("aa", "2"), b("bb", "1"), (format!("{DATA}cc"), json!({})), b("c", "9")], &[]);
        let c = open_ro(&db).unwrap();
        assert_eq!(Ide::with_bubbles(&c).unwrap(), ["aa", "bb", "c"]);
    }
}
