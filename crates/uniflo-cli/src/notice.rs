//! A one-box reminder after a command when a newer uniflo is published.
//!
//! People at a terminal get it on every run: the newest known version is cached
//! (`update-check.json` in the cache directory) and refreshed in a background thread at most once
//! an hour (after a failed try, every ten minutes), so the command itself never waits for the
//! network beyond a short grace period at the end. Output goes to stderr, only when both stdout and
//! stderr are terminals, so pipes, scripts, hooks and agents never see it. `UNIFLO_NO_UPDATE_CHECK=1`
//! (or `CI`) switches it off; the daemon's own hourly check (ADR-0009) is separate.

use crate::i18n::tr;
use crate::ui;
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use uniflo_core::update::{self, is_newer};
use uniflo_core::util::now_ms;

const FRESH_MS: i64 = 60 * 60_000;
const RETRY_MS: i64 = 10 * 60_000;
/// How long the end of a command waits for a check that is still running.
const GRACE: Duration = Duration::from_millis(1500);
const FILE: &str = "update-check.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    /// Last check that got an answer.
    #[serde(default)]
    checked_at: i64,
    /// Last attempt, answered or not.
    #[serde(default)]
    tried_at: i64,
    #[serde(default)]
    latest: Option<String>,
    #[serde(default)]
    prerelease: Option<String>,
}

fn load(dir: &Path) -> Cache {
    std::fs::read(dir.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(dir: &Path, c: &Cache) {
    let _ = std::fs::create_dir_all(dir);
    let tmp = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    if let Ok(b) = serde_json::to_vec(c)
        && std::fs::write(&tmp, b).is_ok()
        && std::fs::rename(&tmp, dir.join(FILE)).is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Whether the cache is old enough to ask crates.io again.
fn due(c: &Cache, now: i64) -> bool {
    now - c.checked_at >= FRESH_MS && now - c.tried_at >= RETRY_MS
}

/// The box to print for `current`, if the cache knows a newer release.
fn message(c: &Cache, current: &str) -> Option<String> {
    if let Some(latest) = c.latest.as_deref().filter(|v| is_newer(v, current)) {
        return Some(ui::note(
            &ui::pink("✦"),
            &tr!("uniflo {latest} 已发布（当前 {current}）", "uniflo {latest} is out (you have {current})"),
            &tr!(
                "运行 `uniflo update` 升级 · 关闭提醒：UNIFLO_NO_UPDATE_CHECK=1",
                "run `uniflo update` to upgrade · silence: UNIFLO_NO_UPDATE_CHECK=1"
            ),
        ));
    }
    let pre = c.prerelease.as_deref().filter(|v| is_newer(v, current))?;
    Some(format!(
        "  {}",
        ui::dim(&tr!(
            "预发布版 {pre} 可试用：`uniflo update --pre`（不保证稳定）",
            "prerelease {pre} is available: `uniflo update --pre` (not guaranteed stable)"
        ))
    ))
}

/// Terminals only, and only when nothing asked for quiet.
fn enabled() -> bool {
    let off = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
    std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
        && !off("UNIFLO_NO_UPDATE_CHECK")
        && !off("CI")
        && !std::env::args().any(|a| a == "--json")
}

pub struct Notice {
    cache: Option<Cache>,
    refresh: Option<mpsc::Receiver<Cache>>,
}

/// Read the cache and, when it is due, start refreshing it in the background.
pub fn begin(wanted: bool) -> Notice {
    if !wanted || cfg!(test) || !enabled() {
        return Notice { cache: None, refresh: None };
    }
    let dir = uniflo_core::paths::cache_dir();
    let cache = load(&dir);
    let now = now_ms();
    if !due(&cache, now) {
        return Notice { cache: Some(cache), refresh: None };
    }
    save(&dir, &Cache { tried_at: now, ..cache.clone() });
    Notice { cache: Some(cache.clone()), refresh: Some(spawn_check(dir, cache)) }
}

fn spawn_check(dir: PathBuf, old: Cache) -> mpsc::Receiver<Cache> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let info = update::check();
        let now = now_ms();
        let fresh = if info.error.is_none() {
            Cache { checked_at: now, tried_at: now, latest: info.latest, prerelease: info.latest_prerelease }
        } else {
            Cache { tried_at: now, ..old }
        };
        save(&dir, &fresh);
        let _ = tx.send(fresh);
    });
    rx
}

impl Notice {
    /// Print the reminder, if any, once the command is done.
    pub fn show(self) {
        let cache = match self.refresh {
            Some(rx) => rx.recv_timeout(GRACE).ok().or(self.cache),
            None => self.cache,
        };
        if let Some(text) = cache.and_then(|c| message(&c, update::current_version())) {
            eprintln!("\n{text}\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(checked: i64, tried: i64, latest: Option<&str>, pre: Option<&str>) -> Cache {
        Cache { checked_at: checked, tried_at: tried, latest: latest.map(Into::into), prerelease: pre.map(Into::into) }
    }

    #[test]
    fn checks_hourly_and_retries_failures_after_ten_minutes() {
        let h = 60 * 60_000;
        assert!(due(&Cache::default(), h), "never checked");
        assert!(!due(&cache(1_000, 1_000, None, None), 1_000 + h - 1), "fresh answer");
        assert!(due(&cache(1_000, 1_000, None, None), 1_000 + h));
        assert!(
            !due(&cache(1_000, 1_000 + 2 * h, None, None), 1_000 + 2 * h + 9 * 60_000),
            "a recent failed try waits"
        );
        assert!(due(&cache(1_000, 1_000 + 2 * h, None, None), 1_000 + 2 * h + 10 * 60_000));
    }

    #[test]
    fn reminds_only_about_something_newer() {
        let m = message(&cache(1, 1, Some("0.3.0"), None), "0.2.1").unwrap();
        assert!(m.contains("0.3.0") && m.contains("0.2.1") && m.contains("uniflo update"), "{m}");
        assert!(message(&cache(1, 1, Some("0.2.1"), None), "0.2.1").is_none(), "already current");
        assert!(message(&cache(1, 1, Some("0.2.0"), None), "0.2.1").is_none(), "older is not an update");
        assert!(message(&cache(1, 1, None, None), "0.2.1").is_none(), "nothing known");
        let pre = message(&cache(1, 1, Some("0.2.1"), Some("0.3.0-rc.1")), "0.2.1").unwrap();
        assert!(pre.contains("0.3.0-rc.1") && pre.contains("--pre"), "{pre}");
    }

    #[test]
    fn the_cache_survives_a_round_trip_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Cache::default());
        let c = cache(5, 6, Some("0.3.0"), Some("0.4.0-rc.1"));
        save(dir.path(), &c);
        assert_eq!(load(dir.path()), c);
        std::fs::write(dir.path().join(FILE), b"not json").unwrap();
        assert_eq!(load(dir.path()), Cache::default());
    }
}
