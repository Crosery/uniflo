//! Update check against the crates.io sparse index.
//!
//! Transport is the system `curl` (ships with macOS and Windows 10 1803+), like the
//! `ps`/`lsof` shells in [`crate::procs`]: the tree stays free of TLS/HTTP deps.
//! The sparse index is NDJSON, one object per published version.

use serde::Serialize;

/// crates.io sparse-index path for the `uniflo` crate (2-letter namespace);
/// `UNIFLO_UPDATE_INDEX_URL` replaces it (tests).
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

/// Highest non-yanked versions on crates.io, split by channel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Releases {
    /// Newest stable release (`X.Y.Z`, no prerelease suffix).
    pub stable: Option<String>,
    /// Newest prerelease (`X.Y.Z-rc.N`); informational only — never an automatic
    /// upgrade target, prereleases don't guarantee stability.
    pub prerelease: Option<String>,
}

/// Pick the highest non-yanked stable and prerelease `vers` from sparse-index NDJSON.
pub fn parse_releases(body: &str) -> Releases {
    let mut out = Releases::default();
    for line in body.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if obj.get("yanked").and_then(|y| y.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(vers) = obj.get("vers").and_then(|v| v.as_str()) else { continue };
        if version_key(vers).is_none() {
            continue;
        }
        let slot = if vers.contains('-') { &mut out.prerelease } else { &mut out.stable };
        if slot.as_deref().is_none_or(|b| is_newer(vers, b)) {
            *slot = Some(vers.to_owned());
        }
    }
    out
}
/// Outcome of one update check, surfaced in [`crate::engine::Stats`] and `/v1/stats`.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    /// Highest non-yanked **stable** version on crates.io; `None` when the fetch failed.
    /// Prereleases never land here — the default upgrade path is stable-only.
    pub latest: Option<String>,
    /// True only when a stable `latest` is newer than `current`.
    pub available: bool,
    /// Newest non-yanked prerelease strictly newer than both `current` and `latest`,
    /// detected for information only; installing it must be an explicit user choice.
    pub latest_prerelease: Option<String>,
    pub checked_at: i64,
    pub error: Option<String>,
}

/// Fetch the sparse index with the system curl. Err carries a human-readable reason
/// (missing curl, network failure, timeout) — never a panic path for the daemon.
pub fn fetch_releases() -> Result<Releases, String> {
    #[cfg(windows)]
    let bin = "curl.exe";
    #[cfg(not(windows))]
    let bin = "curl";
    let url = std::env::var("UNIFLO_UPDATE_INDEX_URL").ok().filter(|s| !s.is_empty());
    let out = std::process::Command::new(bin)
        .args(["-fsS", "--max-time", "15", "--user-agent", &format!("uniflo/{}", current_version())])
        .arg(url.as_deref().unwrap_or(INDEX_URL))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{bin} unavailable: {e}"))?;
    if !out.status.success() {
        return Err(format!("crates.io index fetch failed (exit {:?})", out.status.code()));
    }
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    let rel = parse_releases(&body);
    if rel.stable.is_none() && rel.prerelease.is_none() {
        return Err("no published versions found".to_owned());
    }
    Ok(rel)
}

/// Judge a release pair against the running version: `available` is driven by the
/// stable channel only; a prerelease is announced when it is ahead of both the
/// current version and the recommended stable, and never before it.
pub fn evaluate(current: &str, rel: &Releases) -> UpdateInfo {
    let available = rel.stable.as_deref().is_some_and(|s| is_newer(s, current));
    let newer_than_stable =
        rel.stable.as_deref().is_none_or(|s| rel.prerelease.as_deref().is_some_and(|p| is_newer(p, s)));
    let newer_than_current = rel.prerelease.as_deref().is_some_and(|p| is_newer(p, current));
    let latest_prerelease = if newer_than_stable && newer_than_current { rel.prerelease.clone() } else { None };
    UpdateInfo {
        current: current.to_owned(),
        latest: rel.stable.clone(),
        available,
        latest_prerelease,
        checked_at: crate::util::now_ms(),
        error: None,
    }
}

/// One blocking update check; never fails, errors land in `UpdateInfo::error`.
pub fn check() -> UpdateInfo {
    let current = current_version().to_owned();
    match fetch_releases() {
        Ok(rel) => evaluate(&current, &rel),
        Err(error) => UpdateInfo {
            current,
            latest: None,
            available: false,
            latest_prerelease: None,
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
    fn sparse_index_skips_yanked_and_splits_channels() {
        let body = concat!(
            "{\"name\":\"uniflo\",\"vers\":\"0.1.0\",\"yanked\":false}\n",
            "{\"name\":\"uniflo\",\"vers\":\"0.1.2\",\"yanked\":false}\n",
            "not json\n",
            "{\"name\":\"uniflo\",\"vers\":\"0.1.3\",\"yanked\":false}\n",
            "{\"name\":\"uniflo\",\"vers\":\"0.1.4-rc.1\",\"yanked\":false}\n",
            "{\"name\":\"uniflo\",\"vers\":\"0.1.4-rc.2\",\"yanked\":false}\n",
            "{\"name\":\"uniflo\",\"vers\":\"9.9.9\",\"yanked\":true}\n",
            "{\"name\":\"uniflo\",\"vers\":\"9.9.9-rc.1\",\"yanked\":true}\n"
        );
        let rel = parse_releases(body);
        assert_eq!(rel.stable.as_deref(), Some("0.1.3"));
        assert_eq!(rel.prerelease.as_deref(), Some("0.1.4-rc.2"));
        let empty = parse_releases("");
        assert_eq!(empty, Releases::default());
    }

    #[test]
    fn prerelease_is_never_an_automatic_update() {
        // Stable 0.1.4 + newer rc 0.1.5-rc.1, running stable 0.1.3:
        // actionable target is the stable; rc is announced as opt-in info.
        let rel = Releases { stable: Some("0.1.4".into()), prerelease: Some("0.1.5-rc.1".into()) };
        let info = evaluate("0.1.3", &rel);
        assert!(info.available);
        assert_eq!(info.latest.as_deref(), Some("0.1.4"));
        assert_eq!(info.latest_prerelease.as_deref(), Some("0.1.5-rc.1"));

        // Running the latest stable with only an rc ahead: NOT available, rc reported.
        let rel = Releases { stable: Some("0.1.4".into()), prerelease: Some("0.1.5-rc.1".into()) };
        let info = evaluate("0.1.4", &rel);
        assert!(!info.available);
        assert_eq!(info.latest.as_deref(), Some("0.1.4"));
        assert_eq!(info.latest_prerelease.as_deref(), Some("0.1.5-rc.1"));

        // Running a prerelease (rc.1) while stable 0.1.3 is the newest stable and the
        // only prerelease ahead is rc.2: stable is NOT newer, so not available; rc.2 shown.
        let rel = Releases { stable: Some("0.1.3".into()), prerelease: Some("0.1.4-rc.2".into()) };
        let info = evaluate("0.1.4-rc.1", &rel);
        assert!(!info.available);
        assert_eq!(info.latest.as_deref(), Some("0.1.3"));
        assert_eq!(info.latest_prerelease.as_deref(), Some("0.1.4-rc.2"));

        // An rc behind the current stable is noise, never announced.
        let rel = Releases { stable: Some("0.1.4".into()), prerelease: Some("0.1.4-rc.1".into()) };
        let info = evaluate("0.1.4", &rel);
        assert!(!info.available);
        assert_eq!(info.latest_prerelease, None);

        // Stable-only channel still works; no prerelease to report.
        let rel = Releases { stable: Some("0.2.0".into()), prerelease: None };
        let info = evaluate("0.1.4", &rel);
        assert!(info.available);
        assert_eq!(info.latest_prerelease, None);
    }
}
