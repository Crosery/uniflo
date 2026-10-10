//! Building blocks for [`crate::Adapter::cleanup_targets`] / [`crate::LineDecoder::cleanup_targets`].
//! Declaring support is one line in the adapter, e.g.
//! `fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> { targets::file(src) }`.

use std::path::{Path, PathBuf};

/// Just the transcript file.
pub fn file(src: &Path) -> Option<Vec<PathBuf>> {
    Some(vec![src.to_path_buf()])
}

/// The transcript first, then its siblings named `<stem>` (the session's own directory, e.g.
/// Claude's `<id>/subagents`) or `<stem>.<…>` (sidecars such as `<id>.meta.json`), and hidden
/// `.<file name>.<…>` lock files.
pub fn with_siblings(src: &Path, stem: &str) -> Option<Vec<PathBuf>> {
    let name = src.file_name()?.to_str()?;
    let (sidecar, lock) = (format!("{stem}."), format!(".{name}."));
    let mut more: Vec<PathBuf> = std::fs::read_dir(src.parent()?)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            (n != name && (n == stem || n.starts_with(&sidecar) || n.starts_with(&lock))).then(|| e.path())
        })
        .collect();
    more.sort();
    let mut out = vec![src.to_path_buf()];
    out.extend(more);
    Some(out)
}

/// The directory holding the transcript, when that directory belongs to this session alone.
pub fn parent_dir(src: &Path) -> Option<Vec<PathBuf>> {
    Some(vec![src.parent()?.to_path_buf()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siblings_take_the_session_dir_sidecars_and_locks_only() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n);
        for f in ["s1.jsonl", "s1.meta.json", ".s1.jsonl.lock", "s10.jsonl", "s1-x.jsonl", "other.jsonl"] {
            std::fs::write(p(f), "").unwrap();
        }
        std::fs::create_dir(p("s1")).unwrap();
        let got = with_siblings(&p("s1.jsonl"), "s1").unwrap();
        assert_eq!(got, vec![p("s1.jsonl"), p(".s1.jsonl.lock"), p("s1"), p("s1.meta.json")]);
        assert_eq!(parent_dir(&p("s1/t.jsonl")).unwrap(), vec![p("s1")]);
    }
}
