//! What Uniflo keeps of cleaned-up sessions, under `<data dir>/archive/` (never a harness's):
//!
//! - `<harness>/<id>.jsonl.zst` — compact transcripts ([`compact`]);
//! - `index.json` — one entry per archived session (snapshot, archive file, moved files);
//! - `cleanup.log.jsonl` — append-only cleanup log; a session's manifest (path, size, mtime,
//!   SHA-256 of every file) is flushed here before anything is moved;
//! - `tombstones.json` — `{path, key, at}` per path a cleanup moved to the trash.
//!
//! A tombstoned path is not read as a source again unless what reappears there is the file that
//! was archived (same size and SHA-256 as the manifest: a restore from the trash). Then the
//! source wins and the tombstone goes; anything else at that path (a partial leftover, a stale
//! copy) stays out of the index, so it never shows up as the session's source.

pub mod compact;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uniflo_schema::{Event, Session};

const INDEX: &str = "index.json";
const TOMBSTONES: &str = "tombstones.json";
const LOG: &str = "cleanup.log.jsonl";

/// One archived session (an `index.json` entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Archived {
    /// Snapshot at cleanup time: `archived: true`, `source` = the archive file.
    pub session: Session,
    /// Archive file, relative to the archive directory.
    pub file: PathBuf,
    pub bytes: u64,
    pub archived_at: i64,
    /// Key of the session whose cleanup produced this entry (itself for the root).
    pub root: String,
    /// Where the transcript lived.
    pub source: PathBuf,
    /// Root entries: every file moved to the trash, and their total size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileRecord>,
    #[serde(default)]
    pub source_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ms: i64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub path: PathBuf,
    pub key: String,
    pub at: i64,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Archived>,
    tombs: HashMap<PathBuf, Tombstone>,
    /// Paths whose content did not match the manifest, by (size, mtime): not re-hashed until they change.
    rejected: HashMap<PathBuf, (u64, i64)>,
}

pub struct ArchiveStore {
    dir: PathBuf,
    state: Mutex<State>,
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<Archived>,
}

impl ArchiveStore {
    /// Load what exists; creates nothing until something is archived.
    pub fn open(dir: PathBuf) -> ArchiveStore {
        let mut st = State::default();
        if let Some(f) = read_json::<IndexFile>(&dir.join(INDEX)) {
            st.entries = f.entries.into_iter().map(|a| (a.session.key.clone(), a)).collect();
        }
        if let Some(ts) = read_json::<Vec<Tombstone>>(&dir.join(TOMBSTONES)) {
            st.tombs = ts.into_iter().map(|t| (t.path.clone(), t)).collect();
        }
        ArchiveStore { dir, state: Mutex::new(st) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join(LOG)
    }

    /// `<harness>/<id>.jsonl.zst`, relative to [`ArchiveStore::dir`].
    pub fn file_for(harness: &str, id: &str) -> PathBuf {
        PathBuf::from(safe_name(harness)).join(format!("{}.jsonl.zst", safe_name(id)))
    }

    pub fn entries(&self) -> Vec<Archived> {
        self.state.lock().unwrap().entries.values().cloned().collect()
    }

    pub fn get(&self, key: &str) -> Option<Archived> {
        self.state.lock().unwrap().entries.get(key).cloned()
    }

    pub fn read(&self, a: &Archived) -> Result<(Session, Vec<Event>)> {
        compact::read(&self.dir.join(&a.file))
    }

    /// Write an archive file durably (temp file, fsync, rename). Returns its size.
    pub fn write_file(&self, rel: &Path, bytes: &[u8]) -> Result<u64> {
        write_atomic(&self.dir.join(rel), bytes)?;
        Ok(bytes.len() as u64)
    }

    /// Append one line to the cleanup log and flush it to disk.
    pub fn log(&self, line: &serde_json::Value) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .with_context(|| format!("open {}", self.log_path().display()))?;
        let mut buf = serde_json::to_vec(line)?;
        buf.push(b'\n');
        f.write_all(&buf)?;
        f.sync_all()?;
        Ok(())
    }

    /// Add (or replace) entries and persist the index.
    pub fn insert(&self, entries: Vec<Archived>) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        for a in entries {
            st.entries.insert(a.session.key.clone(), a);
        }
        self.save_index(&st)
    }

    /// Drop entries and their archive files. Removing a root removes its whole tree.
    pub fn remove(&self, key: &str) -> Result<Vec<Archived>> {
        let mut st = self.state.lock().unwrap();
        let Some(a) = st.entries.get(key) else { return Ok(Vec::new()) };
        let tree = a.root == a.session.key;
        let keys: Vec<String> = st
            .entries
            .values()
            .filter(|e| e.session.key == key || (tree && e.root == key))
            .map(|e| e.session.key.clone())
            .collect();
        let gone: Vec<Archived> = keys.iter().filter_map(|k| st.entries.remove(k)).collect();
        self.save_index(&st)?;
        if tree {
            st.tombs.retain(|_, t| t.key != key);
            self.save_tombs(&st)?;
        }
        drop(st);
        for a in &gone {
            match std::fs::remove_file(self.dir.join(&a.file)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("remove {}", a.file.display()));
                }
                _ => {}
            }
        }
        Ok(gone)
    }

    /// Forget entries whose archive files were just written but whose cleanup failed.
    pub fn discard(&self, keys: &[String]) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let mut files = Vec::new();
        for k in keys {
            if let Some(a) = st.entries.remove(k) {
                files.push(a.file);
            }
        }
        self.save_index(&st)?;
        drop(st);
        for f in files {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
        Ok(())
    }

    // ------------------------------------------------------------ tombstones

    /// In memory only; [`ArchiveStore::save_tombstones`] persists.
    pub fn bury(&self, paths: &[PathBuf], key: &str, at: i64) {
        let mut st = self.state.lock().unwrap();
        for p in paths {
            st.tombs.insert(p.clone(), Tombstone { path: p.clone(), key: key.to_owned(), at });
            st.rejected.remove(p);
        }
    }

    pub fn unbury(&self, paths: &[PathBuf]) {
        let mut st = self.state.lock().unwrap();
        for p in paths {
            st.tombs.remove(p);
        }
    }

    pub fn save_tombstones(&self) -> Result<()> {
        let st = self.state.lock().unwrap();
        self.save_tombs(&st)
    }

    pub fn tombstones(&self) -> Vec<Tombstone> {
        self.state.lock().unwrap().tombs.values().cloned().collect()
    }

    /// The tombstone covering `path` (itself or a directory above it).
    pub fn tombstoned(&self, path: &Path) -> Option<Tombstone> {
        let st = self.state.lock().unwrap();
        if st.tombs.is_empty() {
            return None;
        }
        path.ancestors().find_map(|a| st.tombs.get(a)).cloned()
    }

    /// May `path` (under a tombstone) be read as a source? Only when it is the archived file
    /// itself; then the tombstone is lifted and persisted. Blocking (hashes the file).
    pub fn admit(&self, path: &Path) -> bool {
        let Some(t) = self.tombstoned(path) else { return true };
        let Ok(md) = std::fs::metadata(path) else { return false };
        let stamp = (md.len(), crate::util::file_mtime_ms(&md));
        let want = {
            let st = self.state.lock().unwrap();
            if st.rejected.get(path) == Some(&stamp) {
                return false;
            }
            st.entries.get(&t.key).and_then(|a| a.files.iter().find(|f| f.path == path).cloned())
        };
        let same = want.is_some_and(|f| f.size == md.len() && sha256_file(path).is_ok_and(|h| h == f.sha256));
        let mut st = self.state.lock().unwrap();
        if !same {
            st.rejected.insert(path.to_path_buf(), stamp);
            return false;
        }
        st.tombs.remove(&t.path);
        let _ = self.save_tombs(&st);
        true
    }

    fn save_index(&self, st: &State) -> Result<()> {
        let f = IndexFile { version: 1, entries: st.entries.values().cloned().collect() };
        write_atomic(&self.dir.join(INDEX), &serde_json::to_vec_pretty(&f)?)
    }

    fn save_tombs(&self, st: &State) -> Result<()> {
        let mut v: Vec<&Tombstone> = st.tombs.values().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        write_atomic(&self.dir.join(TOMBSTONES), &serde_json::to_vec_pretty(&v)?)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Option<T> {
    let bytes = std::fs::read(p).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("ignoring unreadable {}: {e}", p.display());
            None
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A file-name-safe rendering of a harness or session id (`%XX` for anything unusual).
fn safe_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            b'.' if !out.is_empty() => out.push('.'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe() {
        assert_eq!(ArchiveStore::file_for("claude", "4f1c-9"), PathBuf::from("claude/4f1c-9.jsonl.zst"));
        assert_eq!(ArchiveStore::file_for("pi", "../x/y"), PathBuf::from("pi/%2E.%2Fx%2Fy.jsonl.zst"));
    }

    #[test]
    fn tombstones_admit_only_the_archived_file() {
        let d = tempfile::tempdir().unwrap();
        let store = ArchiveStore::open(d.path().join("archive"));
        let src = d.path().join("s.jsonl");
        std::fs::write(&src, "original\n").unwrap();
        let rec = FileRecord { path: src.clone(), size: 9, mtime_ms: 0, sha256: sha256_file(&src).unwrap() };
        let a = Archived {
            session: serde_json::from_str(
                r#"{"key":"t:s","harness":"t","id":"s","source":"x","updated_at":1,"status":"idle","status_since":0}"#,
            )
            .unwrap(),
            file: PathBuf::from("t/s.jsonl.zst"),
            bytes: 1,
            archived_at: 5,
            root: "t:s".into(),
            source: src.clone(),
            files: vec![rec],
            source_bytes: 9,
        };
        store.insert(vec![a]).unwrap();
        store.bury(std::slice::from_ref(&src), "t:s", 5);
        store.save_tombstones().unwrap();
        assert!(store.tombstoned(&src.join("nested")).is_some(), "covers paths below it");

        std::fs::write(&src, "a leftover\n").unwrap();
        assert!(!store.admit(&src), "different content stays out");
        std::fs::write(&src, "original\n").unwrap();
        assert!(store.admit(&src), "the archived file itself is a restore");
        assert!(store.tombstoned(&src).is_none());
        let reopened = ArchiveStore::open(d.path().join("archive"));
        assert!(reopened.tombstones().is_empty(), "lifting is persisted");
        assert_eq!(reopened.entries().len(), 1);
    }
}
