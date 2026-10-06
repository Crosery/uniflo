//! Update check against the crates.io sparse index.
//!
//! Transport is the system `curl` (ships with macOS and Windows 10 1803+), like the
//! `ps`/`lsof` shells in [`crate::procs`]: the tree stays free of TLS/HTTP deps.
//! The sparse index is NDJSON, one object per published version.

use serde::Serialize;

/// crates.io sparse-index path for the `uniflo` crate (2-letter namespace).
pub const INDEX_URL: &str = "https://index.crates.io/un/if/uniflo";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Sort key for a version string: release outranks a prerelease of the same number;
/// prerelease identifiers compare lexically (good enough for `rc.1` < `rc.2`, exact
/// SemVer identifier ordering is not needed for a newer/older hint).
fn version_key(v: &str) -> Option<(u64, u64, u64, u8, &str)> {
    let core = v.split('+').next()?;
    let (main, pre) = match core.split_once('-') {
        Some((m, p)) => (m, p),
        None => (core, ""),
    };
    let mut n = main.split('.');
    let major: u64 = n.next()?.parse().ok()?;
    let minor: u64 = n.next()?.parse().ok()?;
    let patch: u64 = n.next()?.parse().ok()?;
    if n.next().is_some() {
        return None;
    }
    // rank 1 = release, 0 = prerelease; empty prerelease sorts as release.
    Some((major, minor, patch, if pre.is_empty() { 1 } else { 0 }, pre))
}

/// True when `latest` is a strictly newer release than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (version_key(latest), version_key(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Pick the highest non-yanked `vers` from sparse-index NDJSON.
pub fn parse_latest(body: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for line in body.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if obj.get("yanked").and_then(|y| y.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(vers) = obj.get("vers").and_then(|v| v.as_str()) else { continue };
        if version_key(vers).is_none() {
            continue;
        }
        if best.as_deref().is_none_or(|b| is_newer(vers, b)) {
            best = Some(vers.to_owned());
        }
    }
    best
}
/// Outcome of one update check, surfaced in [`crate::engine::Stats`] and `/v1/stats`.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    /// Highest non-yanked version on crates.io; `None` when the fetch failed.
    pub latest: Option<String>,
    pub available: bool,
    pub checked_at: i64,
    pub error: Option<String>,
}

/// Fetch the sparse index with the system curl. Err carries a human-readable reason
/// (missing curl, network failure, timeout) — never a panic path for the daemon.
pub fn fetch_latest() -> Result<String, String> {
    #[cfg(windows)]
    let bin = "curl.exe";
    #[cfg(not(windows))]
    let bin = "curl";
    let out = std::process::Command::new(bin)
        .args(["-fsS", "--max-time", "15", "--user-agent", &format!("uniflo/{}", current_version()), INDEX_URL])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{bin} unavailable: {e}"))?;
    if !out.status.success() {
        return Err(format!("crates.io index fetch failed (exit {:?})", out.status.code()));
    }
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    parse_latest(&body).ok_or_else(|| "no published versions found".to_owned())
}

/// One blocking update check; never fails, errors land in `UpdateInfo::error`.
pub fn check() -> UpdateInfo {
    let current = current_version().to_owned();
    match fetch_latest() {
        Ok(latest) => {
            let available = is_newer(&latest, &current);
            UpdateInfo { current, latest: Some(latest), available, checked_at: crate::util::now_ms(), error: None }
        }
        Err(error) => UpdateInfo {
            current,
            latest: None,
            available: false,
            checked_at: crate::util::now_ms(),
            error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_ordering_and_prerelease_rank() {
        assert!(is_newer("0.1.3", "0.1.2"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        // a release is newer than its own prerelease
        assert!(is_newer("0.1.2", "0.1.2-rc.1"));
        assert!(!is_newer("0.1.2-rc.1", "0.1.2"));
        assert!(is_newer("0.1.2-rc.2", "0.1.2-rc.1"));
        // garbage never claims an update
        assert!(!is_newer("not-a-version", "0.1.2"));
        assert!(!is_newer("0.1.3", "garbage"));
    }

    #[test]
    fn sparse_index_skips_yanked_and_picks_highest() {
        let body = concat!(
            "{\"name\":\"uniflo\",\"vers\":\"0.1.0\",\"yanked\":false}\n",
            "{\"name\":\"uniflo\",\"vers\":\"0.1.2\",\"yanked\":false}\n",
            "not json\n",
            "{\"name\":\"uniflo\",\"vers\":\"0.1.3\",\"yanked\":false}\n",
            "{\"name\":\"uniflo\",\"vers\":\"9.9.9\",\"yanked\":true}\n"
        );
        assert_eq!(parse_latest(body).as_deref(), Some("0.1.3"));
        assert_eq!(parse_latest(""), None);
    }
}
