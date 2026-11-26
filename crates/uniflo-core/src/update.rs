//! Update check against the crates.io sparse index and its mirrors.
//!
//! Transport is the system `curl` (ships with macOS and Windows 10 1803+), like the
//! `ps`/`lsof` shells in [`crate::procs`]: the tree stays free of TLS/HTTP deps.
//! The sparse index is NDJSON, one object per published version. Sources and their order come
//! from [`crate::mirror`]: the first group is asked at once, the second when the first is slow or
//! fails, and the newest version any of them reports wins, so a lagging mirror cannot hide a
//! release.

use serde::Serialize;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Sparse-index path of the `uniflo` crate under an index root (2-letter namespace).
pub const INDEX_PATH: &str = "un/if/uniflo";
/// The official sparse-index URL of the crate; `UNIFLO_UPDATE_INDEX_URL` replaces every source
/// with one URL (tests, a private mirror).
pub const INDEX_URL: &str = "https://index.crates.io/un/if/uniflo";

/// How long the first group gets before the second is asked too.
const HEDGE: Duration = Duration::from_secs(2);
/// After the first answer, how long the other running sources still get to report a newer version.
const GRACE: Duration = Duration::from_millis(800);
/// The whole check ends by then; each request also gives up on its own (`--max-time`).
const DEADLINE: Duration = Duration::from_secs(12);

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

/// Pick the highest non-yanked stable and prerelease `vers` from sparse-index NDJSON, plus every
/// non-yanked version string listed by that source.
fn parse_index(body: &str) -> (Releases, Vec<String>) {
    let mut out = Releases::default();
    let mut versions = Vec::new();
    for line in body.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if obj.get("yanked").and_then(|y| y.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(vers) = obj.get("vers").and_then(|v| v.as_str()) else { continue };
        if version_key(vers).is_none() {
            continue;
        }
        versions.push(vers.to_owned());
        let slot = if vers.contains('-') { &mut out.prerelease } else { &mut out.stable };
        if slot.as_deref().is_none_or(|b| is_newer(vers, b)) {
            *slot = Some(vers.to_owned());
        }
    }
    (out, versions)
}

/// Pick the highest non-yanked stable and prerelease `vers` from sparse-index NDJSON.
pub fn parse_releases(body: &str) -> Releases {
    parse_index(body).0
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

/// Measured answer from one sparse-index root during a smart probe check.
#[derive(Debug, Clone)]
pub struct SourceHit {
    pub root: String,
    pub official: bool,
    pub elapsed_ms: u64,
    pub versions: Vec<String>,
}

/// Result of [`check_smart`]: the merged [`UpdateInfo`] plus per-source latency & version hits
/// so `uniflo update` can pick the fastest up-to-date registry over the real network.
#[derive(Debug, Clone)]
pub struct SmartCheck {
    pub info: UpdateInfo,
    pub hits: Vec<SourceHit>,
}

impl SmartCheck {
    pub fn index_probes_for(&self, version: &str) -> Vec<crate::mirror::IndexProbe> {
        self.hits
            .iter()
            .map(|h| crate::mirror::IndexProbe {
                root: h.root.clone(),
                official: h.official,
                elapsed_ms: h.elapsed_ms,
                has_version: h.versions.iter().any(|v| v == version),
            })
            .collect()
    }
}

/// One candidate sparse-index target for [`race_detailed`].
struct Target {
    root: String,
    url: String,
    official: bool,
}

/// Ask the index sources for the published versions. Err carries a human-readable reason
/// (missing curl, every source failed) — never a panic path for the daemon.
pub fn fetch_releases() -> Result<Releases, String> {
    let groups = match std::env::var("UNIFLO_UPDATE_INDEX_URL").ok().filter(|s| !s.is_empty()) {
        Some(url) => [vec![Target { root: String::new(), url, official: false }], Vec::new()],
        None => crate::mirror::index_groups(crate::mirror::region()).map(|g| {
            g.into_iter()
                .map(|root| {
                    let official = root == crate::mirror::OFFICIAL_INDEX;
                    let url = format!("{root}{INDEX_PATH}");
                    Target { root, url, official }
                })
                .collect()
        }),
    };
    race_detailed(&groups, HEDGE, GRACE, DEADLINE).map(|(rel, _)| rel)
}

/// Like [`check`], but in auto mode (when neither `UNIFLO_UPDATE_INDEX_URL` nor `UNIFLO_MIRROR`
/// is pinned) races official crates.io and every mirror in parallel so the caller gets both the
/// newest release and real per-source latency + version freshness measurements.
pub fn check_smart() -> SmartCheck {
    let current = current_version().to_owned();
    let custom_url = std::env::var("UNIFLO_UPDATE_INDEX_URL").ok().filter(|s| !s.is_empty());
    let (groups, grace) = match custom_url {
        Some(url) => ([vec![Target { root: String::new(), url, official: false }], Vec::new()], GRACE),
        None => match crate::mirror::env_override() {
            Some(r) => (
                crate::mirror::index_groups(r).map(|g| {
                    g.into_iter()
                        .map(|root| {
                            let official = root == crate::mirror::OFFICIAL_INDEX;
                            let url = format!("{root}{INDEX_PATH}");
                            Target { root, url, official }
                        })
                        .collect()
                }),
                GRACE,
            ),
            None => {
                // Auto mode: race official + all mirrors simultaneously so we measure real
                // network latency across every candidate in one short round-trip.
                let all = crate::mirror::ALL_INDEXES
                    .iter()
                    .map(|&root| Target {
                        root: root.to_owned(),
                        url: format!("{root}{INDEX_PATH}"),
                        official: root == crate::mirror::OFFICIAL_INDEX,
                    })
                    .collect();
                ([all, Vec::new()], Duration::from_millis(900))
            }
        },
    };
    match race_detailed(&groups, HEDGE, grace, DEADLINE) {
        Ok((rel, hits)) => SmartCheck { info: evaluate(&current, &rel), hits },
        Err(error) => SmartCheck {
            info: UpdateInfo {
                current,
                latest: None,
                available: false,
                latest_prerelease: None,
                checked_at: crate::util::now_ms(),
                error: Some(error),
            },
            hits: Vec::new(),
        },
    }
}

/// One running `curl` writing an index body to `out`.
struct Probe {
    root: String,
    url: String,
    official: bool,
    started_at: Instant,
    child: Child,
    out: PathBuf,
}

#[cfg(test)]
fn race(groups: &[Vec<String>; 2], hedge: Duration, grace: Duration, deadline: Duration) -> Result<Releases, String> {
    let targets = [
        groups[0].iter().map(|u| Target { root: String::new(), url: u.clone(), official: false }).collect(),
        groups[1].iter().map(|u| Target { root: String::new(), url: u.clone(), official: false }).collect(),
    ];
    race_detailed(&targets, hedge, grace, deadline).map(|(rel, _)| rel)
}

/// Fetch `groups[0]` at once and `groups[1]` once `hedge` passes without an answer or the first
/// group has failed; merge every answer that arrives within `grace` of the first one.
fn race_detailed(
    groups: &[Vec<Target>; 2],
    hedge: Duration,
    grace: Duration,
    deadline: Duration,
) -> Result<(Releases, Vec<SourceHit>), String> {
    #[cfg(windows)]
    let bin = "curl.exe";
    #[cfg(not(windows))]
    let bin = "curl";
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "uniflo-index-{}-{}-{}",
        std::process::id(),
        crate::util::now_ms(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let agent = format!("uniflo/{}", current_version());
    let limit = deadline.as_secs().max(1).to_string();
    let mut n = 0;
    let mut start = |target: &Target, running: &mut Vec<Probe>, errors: &mut Vec<String>| {
        n += 1;
        let out = dir.join(format!("{n}.ndjson"));
        let started_at = Instant::now();
        let spawned = crate::util::silent_cmd(bin)
            .args(["-fsSL", "--connect-timeout", "5", "--max-time", &limit, "--user-agent", &agent, "-o"])
            .arg(&out)
            .arg(&target.url)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => running.push(Probe {
                root: target.root.clone(),
                url: target.url.clone(),
                official: target.official,
                started_at,
                child,
                out,
            }),
            Err(e) => errors.push(format!("{bin} unavailable: {e}")),
        }
    };
    let t0 = Instant::now();
    let (mut running, mut errors, mut found, mut hits) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for target in &groups[0] {
        start(target, &mut running, &mut errors);
    }
    let mut second = groups[1].is_empty();
    let mut settle = None;
    loop {
        running.retain_mut(|p| {
            let status = match p.child.try_wait() {
                Ok(None) => return true,
                Ok(Some(status)) => status,
                Err(e) => {
                    errors.push(format!("{} ({e})", crate::mirror::host(&p.url)));
                    return false;
                }
            };
            let host = crate::mirror::host(&p.url);
            if !status.success() {
                errors.push(format!("{host} (curl exit {})", status.code().map_or("signal".into(), |c| c.to_string())));
                return false;
            }
            let elapsed_ms = p.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
            let (rel, versions) = parse_index(&std::fs::read_to_string(&p.out).unwrap_or_default());
            if rel == Releases::default() {
                errors.push(format!("{host} (no published versions)"));
            } else {
                found.push(rel);
                if !p.root.is_empty() {
                    hits.push(SourceHit { root: p.root.clone(), official: p.official, elapsed_ms, versions });
                }
            }
            false
        });
        let now = Instant::now();
        if !found.is_empty() {
            let at = *settle.get_or_insert(now + grace);
            if running.is_empty() || now >= at {
                break;
            }
        } else if !second && (running.is_empty() || now >= t0 + hedge) {
            second = true;
            for target in &groups[1] {
                start(target, &mut running, &mut errors);
            }
        } else if running.is_empty() {
            break;
        }
        if now >= t0 + deadline {
            errors.extend(running.iter().map(|p| format!("{} (timed out)", crate::mirror::host(&p.url))));
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    for mut p in running {
        let _ = p.child.kill();
        let _ = p.child.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
    if found.is_empty() {
        return Err(format!("no update source answered: {}", errors.join(", ")));
    }
    Ok((merge(found), hits))
}

/// The newest stable and the newest prerelease across several answers.
fn merge(all: Vec<Releases>) -> Releases {
    let pick = |a: Option<String>, b: Option<String>| match (a, b) {
        (Some(a), Some(b)) => Some(if is_newer(&b, &a) { b } else { a }),
        (a, b) => a.or(b),
    };
    all.into_iter().fold(Releases::default(), |acc, r| Releases {
        stable: pick(acc.stable, r.stable),
        prerelease: pick(acc.prerelease, r.prerelease),
    })
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    const V4: &str = "{\"name\":\"uniflo\",\"vers\":\"0.2.4\",\"yanked\":false}\n";
    const V5: &str = "{\"name\":\"uniflo\",\"vers\":\"0.2.5\",\"yanked\":false}\n";

    /// An index source on 127.0.0.1 answering every request with `body` after `delay`; counts requests.
    fn source(body: &'static str, delay: Duration) -> (String, Arc<AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/{INDEX_PATH}", l.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for mut s in l.incoming().map_while(Result::ok) {
                h.fetch_add(1, SeqCst);
                std::thread::spawn(move || {
                    let _ = s.read(&mut [0u8; 4096]);
                    std::thread::sleep(delay);
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        (url, hits)
    }

    /// A URL nothing listens on.
    fn closed() -> String {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("http://127.0.0.1:{port}/{INDEX_PATH}")
    }

    const LONG: Duration = Duration::from_secs(10);

    #[test]
    fn a_quick_first_group_answers_alone() {
        let (a, _) = source(V5, Duration::ZERO);
        let (b, b_hits) = source(V4, Duration::ZERO);
        let rel = race(&[vec![a], vec![b]], Duration::from_secs(2), Duration::from_millis(100), LONG).unwrap();
        assert_eq!(rel.stable.as_deref(), Some("0.2.5"));
        assert_eq!(b_hits.load(SeqCst), 0, "the second group was asked although the first answered");
    }

    #[test]
    fn a_slow_first_group_is_hedged() {
        let (slow, _) = source(V5, Duration::from_secs(5));
        let (fast, fast_hits) = source(V4, Duration::ZERO);
        let t0 = Instant::now();
        let rel =
            race(&[vec![slow], vec![fast]], Duration::from_millis(200), Duration::from_millis(100), LONG).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(3), "took {:?}", t0.elapsed());
        assert_eq!(rel.stable.as_deref(), Some("0.2.4"));
        assert_eq!(fast_hits.load(SeqCst), 1);
    }

    #[test]
    fn a_failed_first_group_falls_through_without_waiting() {
        let (b, _) = source(V5, Duration::ZERO);
        let t0 = Instant::now();
        // Windows takes about 2 s to report a refused localhost connection; still well under the hedge.
        let rel = race(&[vec![closed()], vec![b]], Duration::from_secs(8), Duration::from_millis(100), LONG).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
        assert_eq!(rel.stable.as_deref(), Some("0.2.5"));
    }

    #[test]
    fn a_lagging_mirror_does_not_hide_a_release() {
        let (lagging, _) = source(V4, Duration::ZERO);
        let (current, _) = source(V5, Duration::from_millis(300));
        let rel =
            race(&[vec![lagging, current], vec![]], Duration::from_secs(2), Duration::from_secs(2), LONG).unwrap();
        assert_eq!(rel.stable.as_deref(), Some("0.2.5"));
    }

    #[test]
    fn every_failed_source_is_named() {
        let err = race(&[vec![closed()], vec![closed()]], Duration::from_millis(100), Duration::from_millis(100), LONG)
            .unwrap_err();
        assert_eq!(err.matches("127.0.0.1:").count(), 2, "{err}");
    }

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
