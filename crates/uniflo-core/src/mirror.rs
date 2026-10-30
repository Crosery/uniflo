//! Where updates come from: the official crates.io index and GitHub Releases, or public mirrors
//! that answer from mainland China, where both are often slow or unreachable (ADR-0009).
//!
//! The region is guessed from the local time zone only, never from a network lookup;
//! `UNIFLO_MIRROR=cn|official` overrides it. Every fetch still falls back to the other sources, so
//! a wrong guess costs a little latency, not the update.

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
/// crates.io sparse-index mirrors in mainland China that track crates.io within minutes. On
/// 2026-10-30 TUNA was still a version behind three hours after a release, and SJTU served no
/// sparse index, so neither is listed.
pub const CN_INDEXES: [&str; 2] = ["https://rsproxy.cn/index/", "https://mirrors.ustc.edu.cn/crates.io-index/"];
/// Public GitHub download accelerators, put in front of a GitHub URL when a direct download fails.
pub const GITHUB_ACCELERATORS: [&str; 2] = ["https://ghfast.top/", "https://gh-proxy.com/"];
/// The registry `cargo install` uses in mainland China unless the user's cargo config already
/// replaces crates.io.
pub const CN_CARGO_REGISTRY: &str = "sparse+https://rsproxy.cn/index/";

pub fn region() -> Region {
    match std::env::var("UNIFLO_MIRROR").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "cn" | "china" => Region::China,
        "official" | "global" => Region::Global,
        _ => local_time_zone().as_deref().map_or(Region::Global, zone_region),
    }
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

/// Extra `cargo install` arguments that read crates.io through the mainland China mirror, when the
/// region calls for it and the cargo config under `cargo_home` replaces nothing itself.
pub fn cargo_args(region: Region, cargo_home: &Path) -> Vec<String> {
    let replaced = ["config.toml", "config"]
        .iter()
        .any(|f| std::fs::read_to_string(cargo_home.join(f)).is_ok_and(|s| s.contains("replace-with")));
    if region != Region::China || replaced {
        return Vec::new();
    }
    vec![
        "--config".to_owned(),
        "source.crates-io.replace-with=\"uniflo-cn\"".to_owned(),
        "--config".to_owned(),
        format!("source.uniflo-cn.registry=\"{CN_CARGO_REGISTRY}\""),
    ]
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
}
