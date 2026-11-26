//! Where updates come from: the official crates.io index and GitHub Releases, or public mirrors
//! that answer faster when direct connections are slow or unreachable (ADR-0009).
//!
//! Priority:
//! 1. Explicit override: `UNIFLO_MIRROR=cn|official` (and user's own cargo `replace-with`).
//! 2. Live network probe: parallel lightweight HTTP probes against the candidate sources,
//!    measuring real end-to-end latency over the machine's current IP/route/proxy and verifying
//!    that a cargo mirror already carries the target version.
//! 3. System time zone fallback (`Asia/Shanghai` etc. → China mirrors, else official) when live
//!    probing is unavailable or inconclusive.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// Mainland China: mirrors first.
    China,
    /// Everywhere else: official sources first.
    Global,
}

/// The crates.io sparse index.
pub const OFFICIAL_INDEX: &str = "https://index.crates.io/";
/// crates.io sparse-index mirrors in mainland China that mirror both the sparse index and `.crate`
/// downloads and track crates.io within minutes. On 2026-10-30 TUNA was still a version behind
/// three hours after a release (and points `dl` back to static.crates.io), and SJTU served no
/// sparse index, so neither is listed.
pub const CN_INDEXES: [&str; 2] = ["https://rsproxy.cn/index/", "https://mirrors.ustc.edu.cn/crates.io-index/"];
/// Every sparse-index root (official first, then mirrors).
pub const ALL_INDEXES: [&str; 3] = [OFFICIAL_INDEX, CN_INDEXES[0], CN_INDEXES[1]];
/// Public GitHub download accelerators, put in front of a GitHub URL when a direct download fails.
pub const GITHUB_ACCELERATORS: [&str; 2] = ["https://ghfast.top/", "https://gh-proxy.com/"];
/// Default mainland China cargo registry when no live probe data is available.
pub const CN_CARGO_REGISTRY: &str = "sparse+https://rsproxy.cn/index/";

/// Prefer official crates.io when it responds quickly on its own, or is nearly as fast as the
/// fastest mirror (avoids unnecessary mirror hops on overseas networks or fast proxies).
const CARGO_OFFICIAL_FAST_MS: u64 = 350;
const CARGO_OFFICIAL_MARGIN_MS: u64 = 150;

/// Same idea for GitHub Releases: prefer first-party GitHub when it is fast enough.
const RELEASE_OFFICIAL_FAST_MS: u64 = 600;
const RELEASE_OFFICIAL_MARGIN_MS: u64 = 200;

/// Explicit `UNIFLO_MIRROR` override (`cn`/`china` or `official`/`global`); unset or `auto`
/// returns `None` so callers run live network probing first and fall back to time zone.
pub fn env_override() -> Option<Region> {
    match std::env::var("UNIFLO_MIRROR").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "cn" | "china" => Some(Region::China),
        "official" | "global" => Some(Region::Global),
        _ => None,
    }
}

/// Time-zone-only region guess (used as fallback when live probing has no result).
pub fn timezone_region() -> Region {
    local_time_zone().as_deref().map_or(Region::Global, zone_region)
}

pub fn region() -> Region {
    env_override().unwrap_or_else(timezone_region)
}

/// Mainland China for its IANA zones (bare, `:`-prefixed or as a zoneinfo path) and the Windows id.
pub fn zone_region(zone: &str) -> Region {
    let z = zone.trim().trim_start_matches(':');
    let z = z.rsplit_once("zoneinfo/").map_or(z, |(_, name)| name);
    let z = z.strip_prefix("posix/").or_else(|| z.strip_prefix("right/")).unwrap_or(z);
    match z {
        "Asia/Shanghai"
        | "Asia/Chongqing"
        | "Asia/Chungking"
        | "Asia/Harbin"
        | "Asia/Urumqi"
        | "Asia/Kashgar"
        | "PRC"
        | "China Standard Time" => Region::China,
        _ => Region::Global,
    }
}

fn local_time_zone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ")
        && !tz.trim().is_empty()
    {
        return Some(tz);
    }
    #[cfg(windows)]
    {
        let out = crate::util::silent_cmd("tzutil").arg("/g").stderr(std::process::Stdio::null()).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
    }
    #[cfg(not(windows))]
    {
        // macOS and most Linux link /etc/localtime into zoneinfo; Debian-style systems that copy
        // the file keep the name in /etc/timezone.
        std::fs::read_link("/etc/localtime")
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| std::fs::read_to_string("/etc/timezone").ok())
    }
}

/// Sparse-index roots for the update check, in two groups: the first is asked at once, the second
/// only when the first is slow or fails.
pub fn index_groups(region: Region) -> [Vec<String>; 2] {
    let official = vec![OFFICIAL_INDEX.to_owned()];
    let cn = CN_INDEXES.iter().map(|s| (*s).to_owned()).collect();
    match region {
        Region::China => [cn, official],
        Region::Global => [official, cn],
    }
}

/// Release roots to download from, in order: GitHub itself, then each accelerator in front of it.
pub fn release_bases(official: &str) -> Vec<String> {
    std::iter::once(official.to_owned()).chain(GITHUB_ACCELERATORS.iter().map(|a| format!("{a}{official}"))).collect()
}

/// True when the user's `$CARGO_HOME/config.toml` (or `config`) already replaces crates.io.
pub fn has_user_cargo_replacement(cargo_home: &Path) -> bool {
    ["config.toml", "config"]
        .iter()
        .any(|f| std::fs::read_to_string(cargo_home.join(f)).is_ok_and(|s| s.contains("replace-with")))
}

/// One candidate registry for `cargo install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoSource {
    /// Sparse-index root URL (e.g. `https://index.crates.io/` or `https://rsproxy.cn/index/`).
    pub index_root: String,
    /// True for official crates.io (uses cargo's built-in source without `--config`).
    pub official: bool,
    /// Measured HTTP round-trip time in milliseconds when chosen from live probes.
    pub latency_ms: Option<u64>,
}

/// Measured live probe result for one sparse-index root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProbe {
    pub root: String,
    pub official: bool,
    pub elapsed_ms: u64,
    /// Whether this source's sparse index already lists the target version.
    pub has_version: bool,
}

/// Why a source list was ordered the way it was, for user-facing status lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickReason {
    /// User set `UNIFLO_MIRROR=cn|official`.
    Override,
    /// Ordered by real network latency & version freshness probes.
    Probed,
    /// Probing had no measurements; fell back to system time zone.
    Timezone,
}

/// Extra `cargo install` arguments to route crates.io through `source` (empty for official).
pub fn cargo_args_for(source: &CargoSource) -> Vec<String> {
    if source.official {
        return Vec::new();
    }
    vec![
        "--config".to_owned(),
        "source.crates-io.replace-with=\"uniflo-cn\"".to_owned(),
        "--config".to_owned(),
        format!("source.uniflo-cn.registry=\"sparse+{}\"", source.index_root),
    ]
}

/// Rank candidate cargo registries: explicit override first, then live network probes (latency +
/// target version freshness, with slight preference for fast official crates.io), then time zone.
pub fn rank_cargo_sources(
    probes: &[IndexProbe],
    override_region: Option<Region>,
    tz_region: Region,
) -> (Vec<CargoSource>, PickReason) {
    let latency_of = |root: &str| probes.iter().filter(|p| p.root == root).map(|p| p.elapsed_ms).min();
    let make = |root: &str| CargoSource {
        index_root: root.to_owned(),
        official: root == OFFICIAL_INDEX,
        latency_ms: latency_of(root),
    };
    let push_unique = |out: &mut Vec<CargoSource>, item: CargoSource| {
        if !out.iter().any(|x| x.index_root == item.index_root) {
            out.push(item);
        }
    };

    let mut out = Vec::new();
    match override_region {
        Some(Region::Global) => {
            push_unique(&mut out, make(OFFICIAL_INDEX));
            let mut mirrors: Vec<&IndexProbe> = probes.iter().filter(|p| !p.official).collect();
            mirrors.sort_by_key(|p| (!p.has_version, p.elapsed_ms));
            for p in mirrors {
                push_unique(&mut out, make(&p.root));
            }
            for root in CN_INDEXES {
                push_unique(&mut out, make(root));
            }
            (out, PickReason::Override)
        }
        Some(Region::China) => {
            let mut mirrors: Vec<&IndexProbe> = probes.iter().filter(|p| !p.official).collect();
            mirrors.sort_by_key(|p| (!p.has_version, p.elapsed_ms));
            for p in mirrors {
                push_unique(&mut out, make(&p.root));
            }
            for root in CN_INDEXES {
                push_unique(&mut out, make(root));
            }
            push_unique(&mut out, make(OFFICIAL_INDEX));
            (out, PickReason::Override)
        }
        None => {
            // Live network probe first: prefer registries confirmed to carry the target version.
            let mut primary: Vec<&IndexProbe> = probes.iter().filter(|p| p.has_version).collect();
            if primary.is_empty() {
                primary = probes.iter().collect();
            }
            if !primary.is_empty() {
                primary.sort_by_key(|p| (p.elapsed_ms, !p.official));
                // If official is fast on its own or nearly as fast as the fastest mirror, prefer it.
                if let Some(off_pos) = primary.iter().position(|p| p.official) {
                    let off_ms = primary[off_pos].elapsed_ms;
                    let best_mirror_ms =
                        primary.iter().filter(|p| !p.official).map(|p| p.elapsed_ms).min().unwrap_or(u64::MAX);
                    if off_ms <= CARGO_OFFICIAL_FAST_MS
                        || off_ms <= best_mirror_ms.saturating_add(CARGO_OFFICIAL_MARGIN_MS)
                    {
                        let off = primary.remove(off_pos);
                        primary.insert(0, off);
                    }
                }
                for p in primary {
                    push_unique(&mut out, make(&p.root));
                }
                // Append any remaining probed (lagging) then unprobed known registries as fallbacks.
                let mut rest_probed: Vec<&IndexProbe> = probes.iter().collect();
                rest_probed.sort_by_key(|p| (p.elapsed_ms, !p.official));
                for p in rest_probed {
                    push_unique(&mut out, make(&p.root));
                }
                let fallback_order: [&str; 3] = match tz_region {
                    Region::China => [CN_INDEXES[0], CN_INDEXES[1], OFFICIAL_INDEX],
                    Region::Global => [OFFICIAL_INDEX, CN_INDEXES[0], CN_INDEXES[1]],
                };
                for root in fallback_order {
                    push_unique(&mut out, make(root));
                }
                return (out, PickReason::Probed);
            }
            // No probe measurements: fall back to system time zone.
            let order: [&str; 3] = match tz_region {
                Region::China => [CN_INDEXES[0], CN_INDEXES[1], OFFICIAL_INDEX],
                Region::Global => [OFFICIAL_INDEX, CN_INDEXES[0], CN_INDEXES[1]],
            };
            for root in order {
                push_unique(&mut out, make(root));
            }
            (out, PickReason::Timezone)
        }
    }
}

/// Order release download roots (`bases[0]` is official GitHub, `bases[1..]` are accelerators)
/// using explicit override first, then measured latency `(base, elapsed_ms)`, then time zone.
pub fn rank_release_bases(
    bases: &[String],
    probes: &[(String, u64)],
    override_region: Option<Region>,
    tz_region: Region,
) -> Vec<String> {
    if bases.len() <= 1 {
        return bases.to_vec();
    }
    let official = &bases[0];
    let mut out = Vec::with_capacity(bases.len());
    let push_unique = |out: &mut Vec<String>, s: &str| {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_owned());
        }
    };
    let mut probed: Vec<(&String, u64)> = bases
        .iter()
        .filter_map(|b| probes.iter().filter(|(u, _)| u == b).map(|(_, ms)| *ms).min().map(|ms| (b, ms)))
        .collect();
    probed.sort_by_key(|(b, ms)| (*ms, *b != official));

    match override_region {
        Some(Region::Global) => {
            push_unique(&mut out, official);
            for (b, _) in probed {
                push_unique(&mut out, b);
            }
            for b in bases {
                push_unique(&mut out, b);
            }
        }
        Some(Region::China) => {
            for (b, _) in probed.iter().filter(|(b, _)| *b != official) {
                push_unique(&mut out, b);
            }
            for b in &bases[1..] {
                push_unique(&mut out, b);
            }
            push_unique(&mut out, official);
        }
        None if !probed.is_empty() => {
            if let Some(off_pos) = probed.iter().position(|(b, _)| *b == official) {
                let off_ms = probed[off_pos].1;
                let best_mirror_ms =
                    probed.iter().filter(|(b, _)| *b != official).map(|(_, ms)| *ms).min().unwrap_or(u64::MAX);
                if off_ms <= RELEASE_OFFICIAL_FAST_MS
                    || off_ms <= best_mirror_ms.saturating_add(RELEASE_OFFICIAL_MARGIN_MS)
                {
                    let off = probed.remove(off_pos);
                    probed.insert(0, off);
                }
            }
            for (b, _) in probed {
                push_unique(&mut out, b);
            }
            for b in bases {
                push_unique(&mut out, b);
            }
        }
        None => match tz_region {
            Region::China => {
                for b in &bases[1..] {
                    push_unique(&mut out, b);
                }
                push_unique(&mut out, official);
            }
            Region::Global => {
                for b in bases {
                    push_unique(&mut out, b);
                }
            }
        },
    }
    out
}

/// Extra `cargo install` arguments that read crates.io through the mainland China mirror, when the
/// region calls for it and the cargo config under `cargo_home` replaces nothing itself.
pub fn cargo_args(region: Region, cargo_home: &Path) -> Vec<String> {
    if region != Region::China || has_user_cargo_replacement(cargo_home) {
        return Vec::new();
    }
    cargo_args_for(&CargoSource { index_root: CN_INDEXES[0].to_owned(), official: false, latency_ms: None })
}

/// `CARGO_HOME`, else `~/.cargo`.
pub fn cargo_home() -> std::path::PathBuf {
    std::env::var_os("CARGO_HOME")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::util::home().join(".cargo"))
}

/// The host of a URL, for messages.
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainland_zones_pick_the_mirrors() {
        for z in [
            "Asia/Shanghai",
            ":Asia/Chongqing",
            "/var/db/timezone/zoneinfo/Asia/Shanghai",
            "../usr/share/zoneinfo/posix/Asia/Urumqi",
            "/usr/share/zoneinfo/PRC",
            "Asia/Shanghai\n",
            "China Standard Time",
        ] {
            assert_eq!(zone_region(z), Region::China, "{z}");
        }
        for z in ["Asia/Hong_Kong", "Asia/Taipei", "Europe/Berlin", "America/New_York", "UTC", "CST-8", ""] {
            assert_eq!(zone_region(z), Region::Global, "{z}");
        }
    }

    #[test]
    fn source_order_follows_the_region() {
        let [first, then] = index_groups(Region::China);
        assert_eq!(first, CN_INDEXES.map(str::to_owned));
        assert_eq!(then, [OFFICIAL_INDEX]);
        let [first, then] = index_groups(Region::Global);
        assert_eq!(first, [OFFICIAL_INDEX]);
        assert_eq!(then.len(), 2);

        let gh = "https://github.com/Crosery/uniflo/releases";
        assert_eq!(
            release_bases(gh),
            [gh.to_owned(), format!("https://ghfast.top/{gh}"), format!("https://gh-proxy.com/{gh}"),]
        );
        assert_eq!(host("https://rsproxy.cn/index/un/if/uniflo"), "rsproxy.cn");
        assert_eq!(host("http://127.0.0.1:7/x"), "127.0.0.1:7");
    }

    #[test]
    fn cargo_uses_the_mirror_only_in_china_without_a_user_replacement() {
        let home = std::env::temp_dir().join(format!("uniflo-mirror-{}-{}", std::process::id(), crate::util::now_ms()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(cargo_args(Region::Global, &home).is_empty());
        let args = cargo_args(Region::China, &home);
        assert_eq!(args.len(), 4);
        assert!(args[3].contains("rsproxy.cn"));
        std::fs::write(home.join("config.toml"), "[source.crates-io]\nreplace-with = 'mine'\n").unwrap();
        assert!(cargo_args(Region::China, &home).is_empty());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn smart_cargo_ranking_prefers_fast_probe_fresh_version_then_timezone() {
        let probe = |root: &str, elapsed_ms, has_version| IndexProbe {
            root: root.to_owned(),
            official: root == OFFICIAL_INDEX,
            elapsed_ms,
            has_version,
        };

        // Direct mainland China: USTC 90ms, rsproxy 190ms, official 850ms → USTC wins, rsproxy backup.
        let (list, why) = rank_cargo_sources(
            &[probe(OFFICIAL_INDEX, 850, true), probe(CN_INDEXES[0], 190, true), probe(CN_INDEXES[1], 90, true)],
            None,
            Region::China,
        );
        assert_eq!(why, PickReason::Probed);
        assert_eq!(list[0].index_root, CN_INDEXES[1]);
        assert_eq!(list[0].latency_ms, Some(90));
        assert_eq!(list[1].index_root, CN_INDEXES[0]);
        assert_eq!(list[2].index_root, OFFICIAL_INDEX);

        // Home Mac with fast proxy (even in China timezone): official 180ms beats mirror 120ms.
        let (list, why) = rank_cargo_sources(
            &[probe(OFFICIAL_INDEX, 180, true), probe(CN_INDEXES[0], 120, true)],
            None,
            Region::China,
        );
        assert_eq!(why, PickReason::Probed);
        assert!(list[0].official);

        // Fresh release not yet synced to mirrors: only official has target version → picks official.
        let (list, why) = rank_cargo_sources(
            &[probe(OFFICIAL_INDEX, 900, true), probe(CN_INDEXES[0], 70, false), probe(CN_INDEXES[1], 60, false)],
            None,
            Region::China,
        );
        assert_eq!(why, PickReason::Probed);
        assert!(list[0].official);

        // Explicit override beats probes.
        let (list, why) = rank_cargo_sources(
            &[probe(OFFICIAL_INDEX, 900, true), probe(CN_INDEXES[1], 60, true)],
            Some(Region::Global),
            Region::China,
        );
        assert_eq!(why, PickReason::Override);
        assert!(list[0].official);

        let (list, why) = rank_cargo_sources(
            &[probe(OFFICIAL_INDEX, 100, true), probe(CN_INDEXES[1], 140, true)],
            Some(Region::China),
            Region::Global,
        );
        assert_eq!(why, PickReason::Override);
        assert_eq!(list[0].index_root, CN_INDEXES[1]);

        // No probes at all: falls back to timezone.
        let (list, why) = rank_cargo_sources(&[], None, Region::China);
        assert_eq!(why, PickReason::Timezone);
        assert_eq!(list[0].index_root, CN_INDEXES[0]);
        let (list, why) = rank_cargo_sources(&[], None, Region::Global);
        assert_eq!(why, PickReason::Timezone);
        assert!(list[0].official);
    }

    #[test]
    fn smart_release_base_ranking_prefers_fast_network_then_timezone() {
        let bases = vec![
            "https://github.com/Crosery/uniflo/releases".to_owned(),
            "https://ghfast.top/https://github.com/Crosery/uniflo/releases".to_owned(),
            "https://gh-proxy.com/https://github.com/Crosery/uniflo/releases".to_owned(),
        ];

        // GitHub unreachable/slow, gh-proxy fastest → gh-proxy first, ghfast second, GitHub fallback.
        let ranked =
            rank_release_bases(&bases, &[(bases[1].clone(), 420), (bases[2].clone(), 180)], None, Region::China);
        assert_eq!(ranked, vec![bases[2].clone(), bases[1].clone(), bases[0].clone()]);

        // GitHub fast (450 ms) even in China timezone → GitHub preferred.
        let ranked =
            rank_release_bases(&bases, &[(bases[0].clone(), 450), (bases[1].clone(), 300)], None, Region::China);
        assert_eq!(ranked[0], bases[0]);

        // No probes → falls back to timezone.
        let ranked_cn = rank_release_bases(&bases, &[], None, Region::China);
        assert_eq!(ranked_cn[0], bases[1]);
        let ranked_global = rank_release_bases(&bases, &[], None, Region::Global);
        assert_eq!(ranked_global[0], bases[0]);
    }
}
