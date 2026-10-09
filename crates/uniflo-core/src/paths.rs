//! Uniflo's own directories (never a harness's): data, config and cache.
//!
//! Platform locations come from `dirs`; with `UNIFLO_HOME` set (tests, sandboxes) the same
//! home-relative path is re-rooted under it, so nothing outside that directory is touched.
//! `UNIFLO_DATA_DIR` / `UNIFLO_CONFIG_DIR` / `UNIFLO_CACHE_DIR` pin a directory outright.

use std::path::{Path, PathBuf};

/// Persistent state Uniflo owns: `pricing/`, archives, … (`~/Library/Application Support/uniflo`).
pub fn data_dir() -> PathBuf {
    pick("UNIFLO_DATA_DIR", dirs::data_dir(), ".local/share")
}

/// User-edited settings (`~/Library/Application Support/uniflo` on macOS, `~/.config/uniflo` on Linux).
pub fn config_dir() -> PathBuf {
    pick("UNIFLO_CONFIG_DIR", dirs::config_dir(), ".config")
}

/// Rebuildable caches (`~/Library/Caches/uniflo`).
pub fn cache_dir() -> PathBuf {
    pick("UNIFLO_CACHE_DIR", dirs::cache_dir(), ".cache")
}

fn pick(env: &str, platform: Option<PathBuf>, fallback: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = match std::env::var_os("UNIFLO_HOME").filter(|p| !p.is_empty()) {
        Some(home) => rerooted(Path::new(&home), platform.as_deref(), dirs::home_dir().as_deref(), fallback),
        None => platform.unwrap_or_else(|| crate::util::home().join(fallback)),
    };
    base.join("uniflo")
}

/// `platform` moved from the real home to `home`; `<home>/<fallback>` when it is not under it.
fn rerooted(home: &Path, platform: Option<&Path>, real_home: Option<&Path>, fallback: &str) -> PathBuf {
    match (platform, real_home) {
        (Some(p), Some(h)) if p.starts_with(h) => home.join(p.strip_prefix(h).unwrap_or(p)),
        _ => home.join(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rerooting_keeps_the_platform_layout() {
        let real = Path::new("/Users/me");
        let mac = Path::new("/Users/me/Library/Application Support");
        assert_eq!(
            rerooted(Path::new("/tmp/h"), Some(mac), Some(real), ".local/share"),
            PathBuf::from("/tmp/h/Library/Application Support")
        );
        assert_eq!(rerooted(Path::new("/tmp/h"), None, Some(real), ".config"), PathBuf::from("/tmp/h/.config"));
        assert_eq!(
            rerooted(Path::new("/tmp/h"), Some(Path::new("/var/x")), Some(real), ".cache"),
            PathBuf::from("/tmp/h/.cache")
        );
    }
}
