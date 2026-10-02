//! Read-only SQLite helpers shared by database-backed adapters.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uniflo_core::util::file_mtime_ms;

/// Open strictly read-only; WAL databases stay consistent and are never written.
pub fn open_ro(path: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open {}", path.display()))?;
    c.busy_timeout(Duration::from_millis(200))?;
    Ok(c)
}

pub fn wal_of(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// (size, mtime) of the `-wal` file, `(0, 0)` when absent.
pub fn wal_sig(db: &Path) -> (u64, i64) {
    std::fs::metadata(wal_of(db)).map_or((0, 0), |m| (m.len(), file_mtime_ms(&m)))
}

/// `x.db`, `x.db-wal`, `x.db-shm` next to `db` → `db`.
pub fn source_for_db(db: &Path, path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix(db.file_name()?.to_str()?)?;
    (matches!(rest, "" | "-wal" | "-shm") && path.parent() == db.parent()).then(|| db.to_path_buf())
}

pub fn columns(c: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut st = c.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let v = st.query_map([], |r| r.get::<_, String>(0))?.flatten().collect();
    Ok(v)
}

/// Column expression, or `NULL` when an older schema lacks it.
pub fn col(cols: &HashSet<String>, name: &str) -> String {
    if cols.contains(name) { name.to_owned() } else { "NULL".to_owned() }
}

pub fn text(r: &Row, i: usize) -> Option<String> {
    match r.get_ref(i).ok()? {
        ValueRef::Text(b) | ValueRef::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Null => None,
    }
}

pub fn nonempty(r: &Row, i: usize) -> Option<String> {
    text(r, i).filter(|s| !s.trim().is_empty())
}

pub fn int(r: &Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(n) => Some(n),
        ValueRef::Real(f) => Some(f as i64),
        ValueRef::Text(b) => std::str::from_utf8(b).ok()?.parse().ok(),
        _ => None,
    }
}

pub fn real(r: &Row, i: usize) -> Option<f64> {
    match r.get_ref(i).ok()? {
        ValueRef::Real(f) => Some(f),
        ValueRef::Integer(n) => Some(n as f64),
        ValueRef::Text(b) => std::str::from_utf8(b).ok()?.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_and_shm_map_to_db() {
        let db = Path::new("/d/x.db");
        assert_eq!(source_for_db(db, Path::new("/d/x.db-wal")).as_deref(), Some(db));
        assert_eq!(source_for_db(db, Path::new("/d/x.db-shm")).as_deref(), Some(db));
        assert_eq!(source_for_db(db, Path::new("/d/x.db")).as_deref(), Some(db));
        assert_eq!(source_for_db(db, Path::new("/d/x.db-journal")), None);
        assert_eq!(source_for_db(db, Path::new("/e/x.db")), None);
    }
}
