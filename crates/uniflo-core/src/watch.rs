//! Recursive filesystem notifications (FSEvents on macOS, inotify on Linux).

use anyhow::Result;
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecursiveMode, Watcher as _};
use std::path::{Path, PathBuf};

pub enum Change {
    Path(PathBuf),
    /// The OS dropped events; callers must rescan.
    Rescan,
}

pub struct Watcher {
    _inner: notify::RecommendedWatcher,
}

pub fn watch(roots: &[PathBuf], on_change: impl Fn(Change) + Send + 'static) -> Result<Watcher> {
    // Dedupe raw roots to avoid redundant OS watches.
    let watched_roots = dedupe_roots(roots);
    // FSEvents reports canonical paths; map them back onto ALL configured roots (both canonical
    // and symlinked aliases).
    let mappings: Vec<(PathBuf, PathBuf)> = roots
        .iter()
        .filter_map(|r| {
            let canon = std::fs::canonicalize(r).ok()?;
            Some((canon, r.clone()))
        })
        .collect();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            if ev.need_rescan() {
                on_change(Change::Rescan);
            }
            if matches!(&ev.kind, EventKind::Access(a) if !matches!(a, AccessKind::Close(AccessMode::Write))) {
                return;
            }
            for p in ev.paths.into_iter().filter(|p| !is_sqlite_shm(p)) {
                map_canonical_path(&mappings, p, |mapped| {
                    on_change(Change::Path(mapped));
                });
            }
        }
        Err(_) => on_change(Change::Rescan),
    })?;
    for r in &watched_roots {
        if !r.exists() {
            continue;
        }
        if let Err(err) = w.watch(r, RecursiveMode::Recursive) {
            tracing::warn!("cannot watch {}: {err}", r.display());
        }
    }
    Ok(Watcher { _inner: w })
}

/// A SQLite WAL index (`x.db-shm`) is rewritten whenever any connection opens or reads the
/// database, our own read-only ones included, and never carries data: a commit always touches
/// `-wal` or the database file. Following it would make every read schedule the next one.
fn is_sqlite_shm(p: &Path) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("-shm"))
}

/// Maps an incoming notification path `p` (typically canonicalized by FSEvents/OS) back
/// to every matching configured root prefix in `mappings`.
///
/// If one or more mappings match `p` (by `canon`), `emit` is invoked for each mapped path.
/// If no mappings match, `p` is emitted unchanged.
fn map_canonical_path(mappings: &[(PathBuf, PathBuf)], p: PathBuf, mut emit: impl FnMut(PathBuf)) {
    let mut matched = false;
    for (canon, orig) in mappings {
        if let Ok(rest) = p.strip_prefix(canon) {
            matched = true;
            emit(orig.join(rest));
        }
    }
    if !matched {
        emit(p);
    }
}

/// Drop roots nested inside other roots (a recursive watch already covers them).
fn dedupe_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut v: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    v.sort();
    v.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for r in v {
        if !out.iter().any(|o| r.starts_with(o)) {
            out.push(r.to_path_buf());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_canonical_path_emits_both_alias_and_canonical_roots() {
        let mappings = vec![
            (PathBuf::from("/real/root"), PathBuf::from("/symlink/alias")),
            (PathBuf::from("/real/root"), PathBuf::from("/real/root")),
            (PathBuf::from("/real/root/nested"), PathBuf::from("/real/root/nested")),
        ];

        // Physical change in /real/root/nested/1.jsonl matches all 3 configured roots
        let mut emitted = Vec::new();
        map_canonical_path(&mappings, PathBuf::from("/real/root/nested/1.jsonl"), |p| {
            emitted.push(p);
        });
        assert_eq!(
            emitted,
            vec![
                PathBuf::from("/symlink/alias/nested/1.jsonl"),
                PathBuf::from("/real/root/nested/1.jsonl"),
                PathBuf::from("/real/root/nested/1.jsonl"),
            ]
        );

        // Physical change only under /real/root/other.jsonl matches the first two
        let mut emitted2 = Vec::new();
        map_canonical_path(&mappings, PathBuf::from("/real/root/other.jsonl"), |p| {
            emitted2.push(p);
        });
        assert_eq!(
            emitted2,
            vec![PathBuf::from("/symlink/alias/other.jsonl"), PathBuf::from("/real/root/other.jsonl"),]
        );
    }

    #[test]
    fn map_canonical_path_fallback_when_no_match() {
        let mappings = vec![(PathBuf::from("/real/root"), PathBuf::from("/symlink/alias"))];
        let mut emitted = Vec::new();
        map_canonical_path(&mappings, PathBuf::from("/unrelated/path/1.jsonl"), |p| {
            emitted.push(p);
        });
        assert_eq!(emitted, vec![PathBuf::from("/unrelated/path/1.jsonl")]);
    }

    #[test]
    fn nested_roots_collapse() {
        let r = dedupe_roots(&["/a/b".into(), "/a".into(), "/c".into(), "/a".into()]);
        assert_eq!(r, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }

    #[test]
    fn sqlite_wal_index_is_not_reported() {
        use std::time::{Duration, Instant};
        assert!(is_sqlite_shm(Path::new("/s/state.db-shm")));
        assert!(!is_sqlite_shm(Path::new("/s/state.db-wal")));
        assert!(!is_sqlite_shm(Path::new("/s/state.db")));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let _w = watch(std::slice::from_ref(&root), move |c| {
            if let Change::Path(p) = c {
                let _ = tx.send(p);
            }
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(root.join("x.db-shm"), b"index").unwrap();
        std::fs::write(root.join("x.db-wal"), b"frame").unwrap();
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !seen.iter().any(|n| n == "x.db-wal") {
            if let Ok(p) = rx.recv_timeout(Duration::from_millis(100)) {
                seen.push(name(&p));
            }
        }
        // A late batch could still carry the index file.
        while let Ok(p) = rx.recv_timeout(Duration::from_millis(500)) {
            seen.push(name(&p));
        }
        assert!(seen.iter().any(|n| n == "x.db-wal"), "{seen:?}");
        assert!(!seen.iter().any(|n| n == "x.db-shm"), "{seen:?}");
    }
}
