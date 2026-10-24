//! One-time conversion from the ADR-0008 single-tier `fts-v1.sqlite` into the new tiered store,
//! run by the writer thread before its normal loop while queries already read the new file. Does
//! not re-read harness source files: the old index already has every session's rows, so
//! conversion is a straight read of `docs` / `sess` and a write into the new tables, newest
//! session first, one transaction per session so an interrupted migration resumes with the
//! sessions it had not yet written.

use super::{cold, store};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default)]
pub struct MigrationResult {
    pub sessions: usize,
    /// Size of the old file (with `-wal` / `-shm`), deleted once every session was converted.
    pub reclaimed_bytes: u64,
    /// False when `on_session` asked to stop: the old file stays for the next start to resume.
    pub complete: bool,
}

/// Whether a v1 file sits next to `new_path` waiting to be converted.
pub fn pending(new_path: &Path) -> bool {
    let old_path = v1_path(new_path);
    old_path != new_path && old_path.exists()
}

/// Where a v1 index would sit next to `new_path` (same directory, old file name).
pub fn v1_path(new_path: &Path) -> PathBuf {
    new_path.with_file_name("fts-v1.sqlite")
}

struct OldSess {
    sid: i64,
    key: String,
    pinned: bool,
    done_upd: Option<i64>,
    done_pos: Option<i64>,
    meta: Option<String>,
}

/// Called after each converted session with its key, its new `sess` row, the event rows written,
/// and how many sessions this run converts in all; returning false stops the migration there.
pub type OnSession<'a> = dyn FnMut(&str, store::SessRow, usize, usize) -> bool + 'a;

/// If a v1 file sits next to `new_path`, convert it into `new` (already-migrated sessions, from a
/// prior interrupted run, are skipped). Afterward the old file (and `-wal` / `-shm`) is deleted.
/// Returns `None` when there was nothing to migrate (no file, unreadable, or not a v1 tag — in the
/// last two cases the file is still deleted, so the caller builds fresh). If the disk fills up
/// mid-way the old file is deleted too: it is only a cache, and the space is what is missing.
pub fn migrate(
    new: &Connection,
    new_path: &Path,
    hot_days: u32,
    now: i64,
    on_session: &mut OnSession<'_>,
) -> Result<Option<MigrationResult>> {
    let old_path = v1_path(new_path);
    // Defensive: `new_path` is already named `fts-v1.sqlite` (e.g. a caller-overridden path).
    // Never treat the live file as its own migration source.
    if !pending(new_path) {
        return Ok(None);
    }
    let before: u64 = std::iter::once(old_path.clone())
        .chain(store::wal_paths(&old_path))
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let old = match Connection::open(&old_path) {
        Ok(c) => c,
        Err(_) => {
            remove_old(&old_path);
            return Ok(None);
        }
    };
    let recognized = (|| -> Result<bool> {
        let tag: Option<String> = old.query_row("SELECT v FROM meta WHERE k = 'tag'", [], |r| r.get(0)).optional()?;
        Ok(tag.as_deref().is_some_and(|t| t.starts_with("uniflo-fts/format1/")))
    })()
    .unwrap_or(false);
    if !recognized {
        remove_old(&old_path);
        return Ok(None);
    }
    let sessions = match list_newest_first(&old) {
        Ok(s) => s,
        Err(_) => {
            remove_old(&old_path);
            return Ok(None);
        }
    };
    let already: HashSet<String> = store::load_sessions(new)?.into_iter().map(|(k, _)| k).collect();
    // A prior run already migrated these.
    let sessions: Vec<(OldSess, i64)> = sessions.into_iter().filter(|(s, _)| !already.contains(&s.key)).collect();
    let total = sessions.len();
    let cutoff = now - hot_days as i64 * store::DAY_MS;
    let mut migrated = 0usize;
    for (s, upd) in sessions {
        let rows = match read_old_rows(&old, s.sid) {
            Ok(r) => r,
            Err(_) => continue, // unreadable row(s): leave it for the post-migration refresh.
        };
        let tx_res: Result<store::SessRow> = (|| {
            new.execute_batch("BEGIN")?;
            let res = write_session(new, &s, &rows, upd, cutoff);
            match &res {
                Ok(_) => new.execute_batch("COMMIT")?,
                Err(_) => new.execute_batch("ROLLBACK")?,
            }
            res
        })();
        let row = match tx_res {
            Ok(row) => row,
            Err(e) => {
                if disk_full(&e) {
                    remove_old(&old_path);
                }
                return Err(e);
            }
        };
        migrated += 1;
        if !on_session(&s.key, row, rows.len(), total) {
            return Ok(Some(MigrationResult { sessions: migrated, reclaimed_bytes: 0, complete: false }));
        }
    }
    remove_old(&old_path);
    Ok(Some(MigrationResult { sessions: migrated, reclaimed_bytes: before, complete: true }))
}

fn disk_full(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>().and_then(rusqlite::Error::sqlite_error_code)
            == Some(rusqlite::ErrorCode::DiskFull)
    })
}

fn write_session(
    new: &Connection,
    s: &OldSess,
    old_rows: &[store::Row],
    upd: i64,
    cutoff: i64,
) -> Result<store::SessRow> {
    let new_sid = store::insert_session(new, &s.key)?;
    // The low 32 bits (seq << 3 | kind) do not depend on the session id; only the sid half changes.
    let remap = |id: i64| (new_sid << 32) | (id & 0xFFFF_FFFF);
    let rows: Vec<store::Row> = old_rows.iter().map(|r| store::Row { id: remap(r.id), ..r.clone() }).collect();
    let cold = upd < cutoff;
    if cold {
        let stats = cold::write_blocks(new, new_sid, &rows)?;
        store::bump_cold(new, stats.rows, stats.tokens)?;
        store::set_cold(new, new_sid, true)?;
        store::set_fingerprint(new, new_sid, cold::fingerprint(&rows))?;
    } else {
        let mut tokens = 0i64;
        for r in &rows {
            store::insert_hot_row(new, new_sid, r)?;
            tokens += store::token_count(&r.text);
        }
        store::bump_hot(new, rows.len() as i64, tokens)?;
    }
    if s.pinned {
        store::pin_raw(new, new_sid, s.meta.as_deref())?;
    }
    let done_pos = s.done_pos.map(|p| p as u64);
    store::finish_pass(new, new_sid, upd, done_pos)?;
    Ok(store::SessRow {
        sid: new_sid,
        done_upd: Some(upd),
        done_pos,
        pinned: s.pinned,
        cold,
        meta: s.meta.as_deref().filter(|_| s.pinned).and_then(|m| serde_json::from_str(m).ok()),
    })
}

fn remove_old(path: &Path) {
    let _ = std::fs::remove_file(path);
    for p in store::wal_paths(path) {
        let _ = std::fs::remove_file(p);
    }
}

/// Every v1 session, newest first by the best timestamp available without re-reading sources:
/// `done_upd` (the engine's `updated_at` as of the old index's last complete pass), falling back
/// to the latest event timestamp it indexed.
fn list_newest_first(old: &Connection) -> Result<Vec<(OldSess, i64)>> {
    let mut st = old.prepare("SELECT sid, key, done_upd, done_pos, pinned, meta FROM sess")?;
    let sessions: Vec<OldSess> = st
        .query_map([], |r| {
            Ok(OldSess {
                sid: r.get(0)?,
                key: r.get(1)?,
                done_upd: r.get(2)?,
                done_pos: r.get(3)?,
                pinned: r.get::<_, i64>(4)? != 0,
                meta: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut max_ts = old.prepare("SELECT max(ts) FROM docs WHERE sid = ?1")?;
    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let upd = match s.done_upd {
            Some(u) => u,
            None => max_ts.query_row([s.sid], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0),
        };
        out.push((s, upd));
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(out)
}

fn read_old_rows(old: &Connection, sid: i64) -> Result<Vec<store::Row>> {
    let mut st = old.prepare_cached("SELECT id, event, ts, pos, text FROM docs WHERE sid = ?1 ORDER BY id")?;
    let rows = st.query_map(params![sid], |r| {
        Ok(store::Row { id: r.get(0)?, event: r.get(1)?, ts: r.get(2)?, pos: r.get(3)?, text: r.get(4)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fts::store::open as open_new;

    /// A v1-format file, built by hand (as a hand-built "old index" fixture, not copied from any
    /// real data): same schema `fts-v1.sqlite` used, tag matching ADR-0008.
    fn build_v1(path: &Path, rows: &[(i64, &str, &str, i64, &str)], pinned: &[(i64, &str)]) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE sess (sid INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, done_upd INTEGER, \
             done_pos INTEGER, pinned INTEGER NOT NULL DEFAULT 0, meta TEXT);
             CREATE TABLE docs (id INTEGER PRIMARY KEY, sid INTEGER NOT NULL, event TEXT NOT NULL, \
             ts INTEGER NOT NULL, pos INTEGER, text TEXT NOT NULL, UNIQUE (sid, event));
             CREATE VIRTUAL TABLE docs_fts USING fts5(text, content='docs', content_rowid='id', \
             tokenize='trigram case_sensitive 0');",
        )
        .unwrap();
        c.execute("INSERT INTO meta (k, v) VALUES ('tag', 'uniflo-fts/format1/uniflo0.1.0/schema1')", []).unwrap();
        let mut sids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for &(sid, key, _, _, _) in rows {
            if sids.insert(sid) {
                // `done_upd` left NULL: tests that care about a specific value set it explicitly
                // afterward (otherwise migration falls back to the latest row's `ts`, see below).
                c.execute("INSERT INTO sess (sid, key) VALUES (?1, ?2)", params![sid, key]).unwrap();
            }
        }
        for (i, &(sid, _, event, ts, text)) in rows.iter().enumerate() {
            let id = store::row_id(sid, i as i64, 1);
            c.execute(
                "INSERT INTO docs (id, sid, event, ts, text) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, sid, event, ts, text],
            )
            .unwrap();
            c.execute("INSERT INTO docs_fts (rowid, text) VALUES (?1, ?2)", params![id, text]).unwrap();
        }
        for &(sid, meta_json) in pinned {
            c.execute("UPDATE sess SET pinned = 1, meta = ?2 WHERE sid = ?1", params![sid, meta_json]).unwrap();
        }
    }

    #[test]
    fn migrates_hot_and_cold_by_age_and_deletes_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("fts-v2.sqlite");
        let old_path = v1_path(&new_path);
        let day = store::DAY_MS;
        let now = 100 * day;
        build_v1(
            &old_path,
            &[
                (1, "t:recent", "e0", now - day, "fresh deploy text"),
                (2, "t:old", "e1", now - 30 * day, "ancient deploy text"),
            ],
            &[],
        );
        {
            let old = Connection::open(&old_path).unwrap();
            old.execute("UPDATE sess SET done_upd = ?1 WHERE sid = 1", params![now - day]).unwrap();
            old.execute("UPDATE sess SET done_upd = ?1 WHERE sid = 2", params![now - 30 * day]).unwrap();
        }
        assert!(old_path.exists());
        let (new, _) = open_new(&new_path, "uniflo-fts/format2/schema1").unwrap();
        let result = migrate(&new, &new_path, 7, now, &mut |_, _, _, _| true).unwrap().unwrap();
        assert_eq!(result.sessions, 2);
        assert!(result.reclaimed_bytes > 0);
        assert!(!old_path.exists() && !store::wal_paths(&old_path)[0].exists());
        let rows = store::load_sessions(&new).unwrap();
        let recent = rows.iter().find(|(k, _)| k == "t:recent").unwrap();
        let old = rows.iter().find(|(k, _)| k == "t:old").unwrap();
        assert!(!recent.1.cold, "recent session stays hot");
        assert!(old.1.cold, "old session goes straight to cold");
        let ct = store::counters(&new).unwrap();
        assert_eq!((ct.hot_rows, ct.cold_rows), (1, 1));
    }

    #[test]
    fn interrupted_migration_resumes_only_unmigrated_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("fts-v2.sqlite");
        let old_path = v1_path(&new_path);
        build_v1(&old_path, &[(1, "t:a", "e0", 1000, "alpha"), (2, "t:b", "e1", 2000, "beta")], &[]);
        let (new, _) = open_new(&new_path, "uniflo-fts/format2/schema1").unwrap();
        // Simulate a crash after session "t:b" (the newer one) was migrated.
        let new_sid = store::insert_session(&new, "t:b").unwrap();
        store::insert_hot_row(
            &new,
            new_sid,
            &store::Row {
                id: store::row_id(new_sid, 0, 1),
                event: "e1".into(),
                ts: 2000,
                pos: None,
                text: "beta".into(),
            },
        )
        .unwrap();
        store::bump_hot(&new, 1, store::token_count("beta")).unwrap();
        let result = migrate(&new, &new_path, 7, 100 * store::DAY_MS, &mut |_, _, _, _| true).unwrap().unwrap();
        assert_eq!(result.sessions, 1, "only the unmigrated session is processed");
        let rows = store::load_sessions(&new).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn stopping_keeps_the_old_file_and_the_next_run_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("fts-v2.sqlite");
        let old_path = v1_path(&new_path);
        build_v1(&old_path, &[(1, "t:a", "e0", 1000, "alpha"), (2, "t:b", "e1", 2000, "beta")], &[]);
        let (new, _) = open_new(&new_path, "uniflo-fts/format2/schema1").unwrap();
        let mut seen = Vec::new();
        let first = migrate(&new, &new_path, 7, 100 * store::DAY_MS, &mut |key, row, rows, total| {
            seen.push((key.to_owned(), row.cold, rows, total));
            false
        })
        .unwrap()
        .unwrap();
        assert!(!first.complete && first.sessions == 1 && first.reclaimed_bytes == 0);
        assert_eq!(seen, vec![("t:b".to_owned(), true, 1, 2)], "newest first, reported as written");
        assert!(old_path.exists(), "an interrupted migration keeps its source");
        let second = migrate(&new, &new_path, 7, 100 * store::DAY_MS, &mut |_, _, _, total| {
            assert_eq!(total, 1, "only what is left");
            true
        })
        .unwrap()
        .unwrap();
        assert!(second.complete && second.sessions == 1 && second.reclaimed_bytes > 0);
        assert!(!old_path.exists());
        assert_eq!(store::load_sessions(&new).unwrap().len(), 2);
    }

    #[test]
    fn unreadable_or_unrecognized_file_is_deleted_without_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("fts-v2.sqlite");
        let old_path = v1_path(&new_path);
        std::fs::write(&old_path, b"not a sqlite file").unwrap();
        let (new, _) = open_new(&new_path, "uniflo-fts/format2/schema1").unwrap();
        let result = migrate(&new, &new_path, 7, 0, &mut |_, _, _, _| true).unwrap();
        assert!(result.is_none());
        assert!(!old_path.exists());
    }

    #[test]
    fn pinned_sessions_carry_their_metadata_and_progress() {
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("fts-v2.sqlite");
        let old_path = v1_path(&new_path);
        let meta = serde_json::json!({
            "key": "t:archived", "harness": "claude", "id": "archived", "source": "synthetic",
            "updated_at": 500, "status": "idle", "status_since": 500
        })
        .to_string();
        build_v1(&old_path, &[(1, "t:archived", "e0", 500, "archived text")], &[(1, meta.as_str())]);
        let (new, _) = open_new(&new_path, "uniflo-fts/format2/schema1").unwrap();
        migrate(&new, &new_path, 7, 0, &mut |_, _, _, _| true).unwrap().unwrap();
        let rows = store::load_sessions(&new).unwrap();
        let (_, row) = rows.into_iter().find(|(k, _)| k == "t:archived").unwrap();
        assert!(row.pinned);
        assert_eq!(row.done_upd, Some(500), "progress carried forward, not reset");
        assert_eq!(row.meta.as_ref().map(|m| m.key.as_str()), Some("t:archived"), "metadata round-trips");
    }
}
