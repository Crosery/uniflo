//! GitHub Copilot CLI session store (`$COPILOT_HOME` or `~/.copilot`, `session-store.db`).
//!
//! `sessions(id, cwd, branch, summary, created_at, updated_at)` carries metadata and
//! `turns(session_id, turn_index, user_message, assistant_response, timestamp)` one finished
//! exchange per row: each row becomes user message, assistant message and turn end. The
//! `turns` rowid is both the follow cursor and the paging `pos`; times are UTC text.

use crate::sqlite::{col, columns, int, nonempty, open_ro, text, wal_sig};
use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, ts_str};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, session_key};

const ID: &str = "copilot";
/// Turns per session sampled by a summary read.
const WINDOW: i64 = 16;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let db = std::env::var_os("COPILOT_HOME")
        .map(|h| PathBuf::from(h).join("session-store.db"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| home().join(".copilot/session-store.db"));
    vec![Arc::new(Copilot { db })]
}

pub struct Copilot {
    db: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    max_turn: i64,
    max_sess: i64,
    wal_size: u64,
    wal_mtime: i64,
}

struct Schema {
    sess: String,
    turns: String,
}

impl Schema {
    fn load(c: &Connection) -> Result<Schema> {
        let s = columns(c, "sessions")?;
        let t = columns(c, "turns")?;
        Ok(Schema {
            sess: format!(
                "SELECT id, {}, {}, {}, {} FROM sessions",
                col(&s, "cwd"),
                col(&s, "summary"),
                col(&s, "created_at"),
                col(&s, "updated_at")
            ),
            turns: format!(
                "SELECT rowid, session_id, {}, {}, {} FROM turns",
                col(&t, "user_message"),
                col(&t, "assistant_response"),
                col(&t, "timestamp")
            ),
        })
    }
}

fn time(r: &Row, i: usize) -> Option<i64> {
    text(r, i).and_then(|s| ts_str(&s).or_else(|| s.parse::<f64>().ok().map(|n| (n * 1000.0) as i64)))
}

fn sess_row(r: &Row) -> (String, MetaPatch) {
    let id = text(r, 0).unwrap_or_default();
    let patch = MetaPatch {
        cwd: nonempty(r, 1),
        title: nonempty(r, 2).map(|t| (2, t)),
        started_at: time(r, 3),
        updated_at: time(r, 4),
        ..Default::default()
    };
    (id, patch)
}

fn ev(sid: &str, id: String, ts: i64, pos: i64, body: Body) -> Event {
    Event {
        id,
        session: session_key(ID, sid),
        ts,
        pos: Some(pos.max(0) as u64),
        partial: false,
        truncated: false,
        body,
    }
}

/// One turn row → user message, assistant message, turn end.
fn expand(r: &Row, batch: &mut Batch) {
    let rowid = int(r, 0).unwrap_or(0);
    let sid = text(r, 1).unwrap_or_default();
    let ts = time(r, 4).unwrap_or(0);
    let mut push = |suffix: &str, body: Body| {
        batch.items.push((sid.clone(), Record::Event(ev(&sid, format!("t{rowid}{suffix}"), ts, rowid, body))));
    };
    if let Some(text) = nonempty(r, 2) {
        push("", Body::UserMessage { text, synthetic: false });
    }
    if let Some(text) = nonempty(r, 3) {
        push(":a", Body::AssistantMessage { text, model: None });
    }
    push(":end", Body::TurnEnd { reason: Some("completed".into()) });
}

fn turns(c: &Connection, sc: &Schema, filter: &str, args: &[&dyn rusqlite::ToSql], batch: &mut Batch) -> Result<()> {
    let mut st = c.prepare(&format!("{} {filter}", sc.turns))?;
    let mut rows = st.query(args)?;
    while let Some(r) = rows.next()? {
        expand(r, batch);
    }
    Ok(())
}

fn sessions(
    c: &Connection,
    sc: &Schema,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(String, MetaPatch)>> {
    let mut st = c.prepare(&format!("{} {filter}", sc.sess))?;
    let v = st.query_map(args, |r| Ok(sess_row(r)))?.flatten().collect();
    Ok(v)
}

impl Copilot {
    fn cursor_for(&self, st: State) -> Cursor {
        let md = std::fs::metadata(&self.db).ok();
        Cursor {
            offset: st.max_turn.max(0) as u64,
            size: md.as_ref().map_or(0, |m| m.len()),
            mtime_ms: md.as_ref().map_or(0, file_mtime_ms),
            state: serde_json::to_value(st).unwrap_or_default(),
        }
    }

    fn state(&self, c: &Connection, src: &Path) -> Result<State> {
        let (wal_size, wal_mtime) = wal_sig(src);
        Ok(State {
            max_turn: c.query_row("SELECT COALESCE(MAX(rowid),0) FROM turns", [], |r| r.get(0))?,
            max_sess: c.query_row("SELECT COALESCE(MAX(rowid),0) FROM sessions", [], |r| r.get(0))?,
            wal_size,
            wal_mtime,
        })
    }

    fn summary(&self, c: &Connection, sc: &Schema, st: State, reset: bool) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        for (id, patch) in sessions(c, sc, "WHERE id IN (SELECT DISTINCT session_id FROM turns)", &[])? {
            batch.items.push((id.clone(), Record::Meta(patch)));
            let mut recent = Batch::default();
            turns(
                c,
                sc,
                "WHERE session_id=?1 AND rowid<=?2 ORDER BY rowid DESC LIMIT ?3",
                &[&id, &st.max_turn, &WINDOW],
                &mut recent,
            )?;
            // Rows came newest first; restore order turn by turn (three events share a rowid).
            recent.items.sort_by_key(|(_, r)| match r {
                Record::Event(e) => e.pos,
                Record::Meta(_) => None,
            });
            batch.items.extend(recent.items);
        }
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: true, reset })
    }

    fn follow(&self, c: &Connection, sc: &Schema, prev: &State, st: State) -> Result<ReadOutput> {
        let mut rows = Batch::default();
        turns(c, sc, "WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid", &[&prev.max_turn, &st.max_turn], &mut rows)?;
        let mut touched: Vec<String> = Vec::new();
        for (sid, _) in &rows.items {
            if !touched.contains(sid) {
                touched.push(sid.clone());
            }
        }
        let mut batch = Batch::default();
        for (id, patch) in sessions(c, sc, "WHERE rowid>?1 AND rowid<=?2", &[&prev.max_sess, &st.max_sess])? {
            // A new session is listed once it has a turn.
            if touched.contains(&id) {
                touched.retain(|t| *t != id);
                batch.items.push((id, Record::Meta(patch)));
            }
        }
        for id in &touched {
            for (sid, patch) in sessions(c, sc, "WHERE id=?1", &[id])? {
                batch.items.push((sid, Record::Meta(patch)));
            }
        }
        batch.items.extend(rows.items);
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: false, reset: false })
    }
}

impl Adapter for Copilot {
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "GitHub Copilot CLI" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.db.parent().filter(|p| p.is_dir()).map(Path::to_path_buf).into_iter().collect()
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        crate::sqlite::source_for_db(&self.db, path)
    }

    fn discover(&self) -> Vec<PathBuf> {
        if self.db.is_file() { vec![self.db.clone()] } else { Vec::new() }
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let sc = Schema::load(&c)?;
        let st = self.state(&c, src)?;
        let prev = cursor.and_then(|c| serde_json::from_value::<State>(c.state.clone()).ok());
        match (cursor, prev) {
            (Some(_), Some(p)) if p.max_turn <= st.max_turn && p.max_sess <= st.max_sess => {
                self.follow(&c, &sc, &p, st)
            }
            (cur, _) => self.summary(&c, &sc, st, cur.is_some()),
        }
    }

    fn read_all(&self, src: &Path, ids: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let sc = Schema::load(&c)?;
        let st = self.state(&c, src)?;
        for (id, patch) in sessions(&c, &sc, "WHERE id IN (SELECT DISTINCT session_id FROM turns)", &[])? {
            sink(&id, Record::Meta(patch));
        }
        for id in ids {
            let mut batch = Batch::default();
            turns(&c, &sc, "WHERE session_id=?1 AND rowid<=?2 ORDER BY rowid", &[id, &st.max_turn], &mut batch)?;
            for (sid, r) in batch.items {
                sink(&sid, r);
            }
        }
        Ok(self.cursor_for(st))
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let c = open_ro(src)?;
        let sc = Schema::load(&c)?;
        let before = q.before.map_or(i64::MAX, |b| b.min(i64::MAX as u64) as i64);
        let n = q.limit.max(1).div_ceil(3) as i64;
        let mut batch = Batch::default();
        turns(
            &c,
            &sc,
            "WHERE session_id=?1 AND rowid<?2 ORDER BY rowid DESC LIMIT ?3",
            &[&session_id, &before, &n],
            &mut batch,
        )?;
        let mut evs: Vec<Event> = batch
            .items
            .into_iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                Record::Meta(_) => None,
            })
            .collect();
        evs.sort_by_key(|e| e.pos);
        Ok(evs)
    }

    fn changed(&self, src: &Path, cursor: &Cursor) -> bool {
        let Ok(md) = std::fs::metadata(src) else { return true };
        if md.len() != cursor.size || file_mtime_ms(&md) != cursor.mtime_ms {
            return true;
        }
        let (size, mtime) = wal_sig(src);
        match serde_json::from_value::<State>(cursor.state.clone()) {
            Ok(st) => st.wal_size != size || st.wal_mtime != mtime,
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, group, kinds};
    use rusqlite::params;
    use uniflo_schema::Status;

    const SCHEMA: &str = "
        CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, branch TEXT, summary TEXT,
            created_at TEXT, updated_at TEXT);
        CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, turn_index INTEGER NOT NULL,
            user_message TEXT, assistant_response TEXT, timestamp TEXT);";

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn turns_become_exchanges_and_follow_by_rowid() {
        let fx = Fixture::new();
        let db = fx.root().join("session-store.db");
        let w = Connection::open(&db).unwrap();
        w.execute_batch(SCHEMA).unwrap();
        w.execute(
            "INSERT INTO sessions VALUES('s1','/w/a',NULL,'main','Fix login','2026-09-01 08:00:00','2026-09-01 08:05:00')",
            [],
        )
        .unwrap();
        w.execute(
            "INSERT INTO sessions VALUES('s2','/w/b',NULL,'dev','','2026-09-02 09:00:00','2026-09-02 09:00:00')",
            [],
        )
        .unwrap();
        w.execute("INSERT INTO sessions VALUES('s3','/w/c',NULL,'dev','empty','2026-09-02 09:00:00',NULL)", [])
            .unwrap();
        let turn = |sid: &str, i: i64, u: &str, a: &str, t: &str| {
            w.execute(
                "INSERT INTO turns(session_id,turn_index,user_message,assistant_response,timestamp) VALUES(?1,?2,?3,?4,?5)",
                params![sid, i, u, a, t],
            )
            .unwrap();
        };
        turn("s1", 0, "why does login fail", "The token expired.", "2026-09-01 08:01:00");
        turn("s1", 1, "fix it", "Refreshed the token.", "2026-09-01 08:04:00");
        turn("s2", 0, "hello", "Hi!", "2026-09-02 09:00:30");
        let before = files(fx.root());
        let a = Copilot { db: db.clone() };
        let out = a.read(&db, None).unwrap();
        assert!(out.summary);
        let g = group(out.batch);
        assert_eq!(g.keys().collect::<Vec<_>>(), ["s1", "s2"], "a session without turns is not listed");
        let s1 = &g["s1"];
        assert_eq!(
            kinds(&s1.events),
            ["user_message", "assistant_message", "turn_end", "user_message", "assistant_message", "turn_end"]
        );
        assert!(matches!(&s1.events[0].body, Body::UserMessage { text, .. } if text == "why does login fail"));
        assert_eq!(s1.events[0].ts, ts_str("2026-09-01 08:01:00").unwrap());
        assert_eq!(s1.status(), Status::Idle);
        assert_eq!(s1.meta.title.as_deref(), Some("Fix login"));
        assert_eq!(s1.meta.cwd.as_deref(), Some("/w/a"));
        assert_eq!(s1.meta.started_at, ts_str("2026-09-01 08:00:00"));
        assert!(g["s2"].meta.title.is_none(), "empty summary is no title");
        assert!(!a.changed(&db, &out.cursor));

        turn("s2", 1, "and now?", "Done.", "2026-09-02 09:01:00");
        assert!(a.changed(&db, &out.cursor));
        let out2 = a.read(&db, Some(&out.cursor)).unwrap();
        assert!(!out2.summary && !out2.reset);
        let evs: Vec<&Event> = out2
            .batch
            .items
            .iter()
            .filter_map(|(_, r)| if let Record::Event(e) = r { Some(e) } else { None })
            .collect();
        assert_eq!(
            kinds(&evs.iter().map(|e| (*e).clone()).collect::<Vec<_>>()),
            ["user_message", "assistant_message", "turn_end"]
        );
        assert!(evs.iter().all(|e| e.pos == Some(4) && e.session == "copilot:s2"), "only the appended row");

        let h = a.history(&db, "s1", &HistoryQuery { before: None, limit: 3 }).unwrap();
        assert_eq!(h.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["t2", "t2:a", "t2:end"]);
        let h2 = a.history(&db, "s1", &HistoryQuery { before: Some(2), limit: 3 }).unwrap();
        assert_eq!(h2[0].id, "t1");

        // Reads never write: no journal / wal / shm appears, and the connection refuses writes.
        assert_eq!(files(fx.root()), before);
        let ro = open_ro(&db).unwrap();
        assert!(ro.execute("DELETE FROM turns", []).is_err());
    }
}
