//! Recursive filesystem notifications (FSEvents on macOS, inotify on Linux).

use anyhow::Result;
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
    let roots = dedupe_roots(roots);
    // FSEvents reports canonical paths; map them back onto the configured (possibly symlinked) roots.
    let aliases: Vec<(PathBuf, PathBuf)> =
        roots.iter().filter_map(|r| std::fs::canonicalize(r).ok().filter(|c| c != r).map(|c| (c, r.clone()))).collect();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            if ev.need_rescan() {
                on_change(Change::Rescan);
            }
            for p in ev.paths {
                on_change(Change::Path(unalias(&aliases, p)));
            }
        }
        Err(_) => on_change(Change::Rescan),
    })?;
    for r in &roots {
        if let Err(err) = w.watch(r, RecursiveMode::Recursive) {
            tracing::warn!("cannot watch {}: {err}", r.display());
        }
    }
    Ok(Watcher { _inner: w })
}

fn unalias(aliases: &[(PathBuf, PathBuf)], p: PathBuf) -> PathBuf {
    for (canon, orig) in aliases {
        if let Ok(rest) = p.strip_prefix(canon) {
            return orig.join(rest);
        }
    }
    p
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
    fn canonical_paths_map_back() {
        let a = vec![(PathBuf::from("/private/var/x"), PathBuf::from("/var/x"))];
        assert_eq!(unalias(&a, "/private/var/x/s/1.jsonl".into()), PathBuf::from("/var/x/s/1.jsonl"));
        assert_eq!(unalias(&a, "/other/1.jsonl".into()), PathBuf::from("/other/1.jsonl"));
    }

    #[test]
    fn nested_roots_collapse() {
        let r = dedupe_roots(&["/a/b".into(), "/a".into(), "/c".into(), "/a".into()]);
        assert_eq!(r, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }
}
