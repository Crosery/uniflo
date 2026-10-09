//! On-disk index cache: cursors + session snapshots, so a restart only reads new bytes.

use crate::adapter::Cursor;
use crate::status::StatusTracker;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uniflo_schema::Session;

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheFile {
    pub tag: String,
    pub sources: Vec<CachedSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSource {
    pub harness: String,
    pub path: PathBuf,
    pub cursor: Cursor,
    pub sessions: Vec<CachedSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSession {
    pub session: Session,
    pub tracker: StatusTracker,
    pub title_rank: u8,
}

/// Uniflo's cache directory (`~/Library/Caches/uniflo` on macOS). With `UNIFLO_HOME` set it is
/// re-rooted under that home, so sandboxes and tests never touch the real cache.
pub fn dir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    let base = match std::env::var_os("UNIFLO_HOME").map(PathBuf::from) {
        Some(h) => match dirs::home_dir().and_then(|real| base.strip_prefix(real).ok().map(Path::to_path_buf)) {
            Some(rel) => h.join(rel),
            None => h.join(".cache"),
        },
        None => base,
    };
    base.join("uniflo")
}

pub fn default_path() -> PathBuf {
    dir().join("index-v1.json")
}

/// Load a cache written with the same `tag`; anything else is ignored (decoders changed).
pub fn load(path: &Path, tag: &str) -> HashMap<PathBuf, CachedSource> {
    let Ok(bytes) = std::fs::read(path) else { return HashMap::new() };
    match serde_json::from_slice::<CacheFile>(&bytes) {
        Ok(f) if f.tag == tag => f.sources.into_iter().map(|s| (s.path.clone(), s)).collect(),
        _ => HashMap::new(),
    }
}

/// Atomic write (temp file + rename).
pub fn save(path: &Path, file: &CacheFile) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(file)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
