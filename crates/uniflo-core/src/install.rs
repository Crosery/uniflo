//! How this executable was installed, and the self-upgrade of a prebuilt release binary
//! (ADR-0009).
//!
//! `scripts/install.sh` / `install.ps1` record a binary install in `<config dir>/install.json`;
//! `cargo install` puts the executable in `$CARGO_HOME/bin`. Upgrading a binary install downloads
//! the release package and `SHA256SUMS` with the system `curl` (like [`crate::update`]), checks
//! the SHA-256 before unpacking, and swaps the executable by rename, so every failure before the
//! swap leaves the old one untouched.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

/// GitHub Releases of the repository; `UNIFLO_RELEASE_BASE_URL` replaces it (tests, mirrors).
/// Assets live at `<base>/download/v<version>/<file>`, the newest stable at `<base>/latest/download/<file>`.
pub const RELEASE_BASE_URL: &str = "https://github.com/Crosery/uniflo/releases";
/// The installers attached to every release (macOS / Linux, Windows).
pub const INSTALL_SH_URL: &str = "https://github.com/Crosery/uniflo/releases/latest/download/install.sh";
pub const INSTALL_PS1_URL: &str = "https://github.com/Crosery/uniflo/releases/latest/download/install.ps1";
pub const RECORD_FILE: &str = "install.json";
pub const SUMS_FILE: &str = "SHA256SUMS";
/// Every target the release workflow builds (`.github/workflows/release.yml`).
pub const TARGETS: [&str; 5] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// `cargo install uniflo` (the executable sits in a cargo bin directory).
    Cargo,
    /// A release package installed by the install script (`install.json` names this executable).
    Binary,
    Unknown,
}

/// `<config dir>/install.json`, written by the install scripts and kept current by upgrades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub method: String,
    pub target: String,
    pub version: String,
    /// The installed executable.
    pub path: PathBuf,
}

impl InstallRecord {
    pub fn file() -> PathBuf {
        crate::paths::config_dir().join(RECORD_FILE)
    }

    pub fn load(p: &Path) -> Option<InstallRecord> {
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    pub fn save(&self, p: &Path) -> Result<()> {
        let dir = p.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, p).with_context(|| format!("rename to {}", p.display()))
    }
}

pub fn canon(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    match c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) => PathBuf::from(rest),
        None => c,
    }
}

/// Where `cargo install` puts binaries: `$CARGO_HOME/bin` and `~/.cargo/bin`.
pub fn cargo_bin_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("CARGO_HOME")
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(p).join("bin"))
        .into_iter()
        .collect();
    v.push(crate::util::home().join(".cargo/bin"));
    v
}

/// A record naming this very executable means binary; an executable directly in a cargo bin
/// directory means cargo; anything else (a source build, a package manager, a copied file) is
/// unknown and never touched by `uniflo update`.
pub fn detect(exe: &Path, record: Option<&InstallRecord>, cargo_bins: &[PathBuf]) -> Method {
    let exe = canon(exe);
    if record.is_some_and(|r| r.method == "binary" && canon(&r.path) == exe) {
        return Method::Binary;
    }
    if cargo_bins.iter().any(|d| exe.parent() == Some(canon(d).as_path())) {
        return Method::Cargo;
    }
    Method::Unknown
}

/// This build's release target; Linux maps to the static musl build, the only one published.
pub fn host_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// `uniflo-<version>-<target>.tar.gz`, `.zip` for Windows.
pub fn asset_name(version: &str, target: &str) -> String {
    let ext = if target.contains("windows") { "zip" } else { "tar.gz" };
    format!("uniflo-{version}-{target}.{ext}")
}

/// Directory inside the package and the executable in it.
pub fn asset_binary(version: &str, target: &str) -> String {
    let exe = if target.contains("windows") { "uniflo.exe" } else { "uniflo" };
    format!("uniflo-{version}-{target}/{exe}")
}

/// Release roots to download from: `UNIFLO_RELEASE_BASE_URL` alone when set, else GitHub and then
/// the public accelerators in front of it ([`crate::mirror::release_bases`]).
pub fn release_bases() -> Vec<String> {
    match std::env::var("UNIFLO_RELEASE_BASE_URL").ok().filter(|s| !s.is_empty()) {
        Some(base) => vec![base.trim_end_matches('/').to_owned()],
        None => crate::mirror::release_bases(RELEASE_BASE_URL),
    }
}

pub fn download_url(base: &str, version: &str, file: &str) -> String {
    format!("{base}/download/v{version}/{file}")
}

/// The checksum `SHA256SUMS` lists for `name` (`<hex>  <name>`, or `<hex> *<name>` in binary mode).
pub fn listed_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hex, file) = l.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

/// Download `version` for `target` from the first of `bases` that serves it, check it against
/// `SHA256SUMS` (fetched in the same order, so from GitHub itself whenever it answers), unpack it,
/// make sure the new executable runs and reports `version`, then put it in place of `exe`.
/// Returns the base the package came from.
pub fn upgrade_binary<'a>(exe: &Path, target: &str, version: &str, bases: &'a [String]) -> Result<&'a str> {
    let work = Scratch::new()?;
    let name = asset_name(version, target);
    let pkg = work.0.join(&name);
    let sums = work.0.join(SUMS_FILE);
    fetch_any(bases, version, SUMS_FILE, &sums)?;
    let from = fetch_any(bases, version, &name, &pkg)?;
    let listed = std::fs::read_to_string(&sums).context("read SHA256SUMS")?;
    let want = listed_sum(&listed, &name).with_context(|| format!("{SUMS_FILE} has no entry for {name}"))?;
    let got = crate::archive::sha256_file(&pkg).with_context(|| format!("hash {}", pkg.display()))?;
    if got != want {
        bail!("checksum mismatch for {name}: got {got}, {SUMS_FILE} lists {want}");
    }
    let out = work.0.join("unpacked");
    std::fs::create_dir(&out)?;
    unpack(&pkg, &out)?;
    let new = out.join(asset_binary(version, target));
    if !new.is_file() {
        bail!("{name} does not contain {}", asset_binary(version, target));
    }
    let reported = crate::util::silent_cmd(&new)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .with_context(|| format!("run the unpacked {}", asset_binary(version, target)))?;
    if reported != format!("uniflo {version}") {
        bail!("the unpacked executable reports {reported:?}, expected \"uniflo {version}\"");
    }
    swap(&new, exe, if cfg!(windows) { Swap::Aside } else { Swap::Replace })?;
    Ok(from)
}

/// A private temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch> {
        let p = std::env::temp_dir().join(format!("uniflo-update-{}-{}", std::process::id(), crate::util::now_ms()));
        std::fs::create_dir_all(&p).with_context(|| format!("create {}", p.display()))?;
        Ok(Scratch(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Download `file` of `version` from the first base that serves it; returns that base.
fn fetch_any<'a>(bases: &'a [String], version: &str, file: &str, dest: &Path) -> Result<&'a str> {
    let mut errors = Vec::new();
    for base in bases {
        match fetch(&download_url(base, version, file), dest) {
            Ok(()) => return Ok(base),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    bail!("{}", errors.join("; "))
}

/// One download; a source that cannot connect within 8 s or stalls below 10 KB/s for 20 s fails,
/// so the next one gets its turn.
fn fetch(url: &str, dest: &Path) -> Result<()> {
    #[cfg(windows)]
    let bin = "curl.exe";
    #[cfg(not(windows))]
    let bin = "curl";
    let out = crate::util::silent_cmd(bin)
        .args(["-fsSL", "--connect-timeout", "8", "--speed-limit", "10240", "--speed-time", "20", "--max-time", "600"])
        .args(["--user-agent", &format!("uniflo/{}", crate::update::current_version())])
        .arg("-o")
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("{bin} unavailable"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect::<String>();
        bail!("download {url} failed (curl exit {:?}): {msg}", out.status.code());
    }
    Ok(())
}

fn unpack(pkg: &Path, into: &Path) -> Result<()> {
    let zip = pkg.extension().is_some_and(|e| e == "zip");
    let mut cmd = if zip {
        if !cfg!(windows) {
            bail!("zip packages are only unpacked on Windows");
        }
        let mut c = crate::util::silent_cmd("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Expand-Archive -LiteralPath $env:UNIFLO_PKG -DestinationPath $env:UNIFLO_OUT -Force",
        ])
        .env("UNIFLO_PKG", pkg)
        .env("UNIFLO_OUT", into);
        c
    } else {
        let mut c = crate::util::silent_cmd("tar");
        c.arg("-xzf").arg(pkg).arg("-C").arg(into);
        c
    };
    let out =
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output().context("run the unpacker")?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect::<String>();
        bail!("unpack {} failed (exit {:?}): {msg}", pkg.display(), out.status.code());
    }
    Ok(())
}

/// How the new executable takes the old one's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap {
    /// Rename over it; a running process keeps the old inode (Unix).
    Replace,
    /// Windows cannot replace a running exe but can rename it: the old one moves to `<exe>.old`
    /// (or a collision-free backup if `<exe>.old` cannot be removed), which [`remove_aside`]
    /// deletes on the next start.
    Aside,
}

pub fn aside_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_owned();
    name.push(".old");
    exe.with_file_name(name)
}

static BACKUP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Only the legacy name or three-component generated names belong to this updater.
fn is_recognized_backup(exe: &Path, file_name: &str) -> bool {
    let Some(exe_name) = exe.file_name().and_then(|n| n.to_str()) else { return false };
    let Some((name, suffix)) = file_name.split_at_checked(exe_name.len()) else { return false };
    if !name.eq_ignore_ascii_case(exe_name) {
        return false;
    }
    if suffix.eq_ignore_ascii_case(".old") {
        return true;
    }
    let Some((marker, rest)) = suffix.split_at_checked(5) else { return false };
    if !marker.eq_ignore_ascii_case(".old-") {
        return false;
    }
    let mut count = 0;
    for part in rest.split('-') {
        if part.is_empty() || !part.bytes().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        count += 1;
    }
    count == 3
}

/// A collision-free backup path for `exe` when `<exe>.old` cannot be removed.
fn unique_backup_path(exe: &Path) -> Result<PathBuf> {
    let dir = exe.parent().context("executable has no parent directory")?;
    let exe_name = exe.file_name().context("executable has no file name")?.to_string_lossy();
    let pid = std::process::id();
    for _ in 0..1000 {
        let now = crate::util::now_ms().max(0) as u64;
        let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!("{exe_name}.old-{now:x}-{pid:x}-{seq:x}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not generate a unique backup path for {}", exe.display());
}

/// Move `exe` aside for replacement, trying `<exe>.old` first and falling back to a
/// collision-free generated backup if the legacy path cannot be removed.
/// Returns the path to which `exe` was moved.
pub fn move_aside(exe: &Path) -> Result<PathBuf> {
    let legacy = aside_path(exe);
    let _ = std::fs::remove_file(&legacy);
    if !legacy.exists() && std::fs::rename(exe, &legacy).is_ok() {
        return Ok(legacy);
    }
    let backup = unique_backup_path(exe)?;
    std::fs::rename(exe, &backup).with_context(|| format!("move {} aside to {}", exe.display(), backup.display()))?;
    Ok(backup)
}

/// Delete what an [`Swap::Aside`] upgrade left behind; still-running leftovers stay until next time.
/// Only removes the exact owned legacy `.old` and strictly recognized generated backup names
/// for `exe`.
pub fn remove_aside(exe: &Path) {
    let _ = std::fs::remove_file(aside_path(exe));
    if let Some(dir) = exe.parent()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            if entry.file_name().to_str().is_some_and(|name| is_recognized_backup(exe, name)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Stage `new` next to `exe` (same filesystem, so the final rename is atomic) and swap it in.
/// On any error `exe` is left as it was.
pub fn swap(new: &Path, exe: &Path, how: Swap) -> Result<()> {
    let dir = exe.parent().context("executable has no parent directory")?;
    let staged = dir.join(format!(".uniflo-new-{}", std::process::id()));
    std::fs::copy(new, &staged).with_context(|| format!("copy the new executable to {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let done = match how {
        Swap::Replace => std::fs::rename(&staged, exe).with_context(|| format!("replace {}", exe.display())),
        Swap::Aside => {
            remove_aside(exe);
            move_aside(exe).and_then(|backup| {
                std::fs::rename(&staged, exe)
                    .with_context(|| format!("put the new {} in place", exe.display()))
                    .inspect_err(|_| {
                        let _ = std::fs::rename(&backup, exe);
                    })
            })
        }
    };
    if done.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("uniflo-install-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn record(path: &Path) -> InstallRecord {
        InstallRecord {
            method: "binary".into(),
            target: "aarch64-apple-darwin".into(),
            version: "0.1.4".into(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn method_follows_record_then_cargo_dir() {
        let d = tmp("detect");
        let cargo_bin = d.join("cargo/bin");
        let local_bin = d.join("local/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        let in_cargo = cargo_bin.join("uniflo");
        let in_local = local_bin.join("uniflo");
        std::fs::write(&in_cargo, "").unwrap();
        std::fs::write(&in_local, "").unwrap();
        let bins = [cargo_bin.clone()];

        assert_eq!(detect(&in_cargo, None, &bins), Method::Cargo);
        assert_eq!(detect(&in_local, Some(&record(&in_local)), &bins), Method::Binary);
        assert_eq!(detect(&in_local, None, &bins), Method::Unknown);
        // a record for another executable does not make this one a binary install
        assert_eq!(detect(&in_local, Some(&record(&in_cargo)), &bins), Method::Unknown);
        // only directly inside the bin directory
        let nested = cargo_bin.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(detect(&nested.join("uniflo"), None, &bins), Method::Unknown);
        // paths compare after resolving `..` and symlinks
        let dotted = d.join("local/../local/bin/uniflo");
        assert_eq!(detect(&dotted, Some(&record(&in_local)), &bins), Method::Binary);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn asset_names_per_target() {
        let names: Vec<String> = TARGETS.iter().map(|t| asset_name("0.1.5", t)).collect();
        assert_eq!(
            names,
            [
                "uniflo-0.1.5-aarch64-apple-darwin.tar.gz",
                "uniflo-0.1.5-x86_64-apple-darwin.tar.gz",
                "uniflo-0.1.5-x86_64-unknown-linux-musl.tar.gz",
                "uniflo-0.1.5-aarch64-unknown-linux-musl.tar.gz",
                "uniflo-0.1.5-x86_64-pc-windows-msvc.zip",
            ]
        );
        assert_eq!(asset_binary("0.1.5", "x86_64-pc-windows-msvc"), "uniflo-0.1.5-x86_64-pc-windows-msvc/uniflo.exe");
        assert_eq!(asset_binary("0.1.5", "aarch64-apple-darwin"), "uniflo-0.1.5-aarch64-apple-darwin/uniflo");
        assert!(host_target().is_none_or(|t| TARGETS.contains(&t)));
        assert_eq!(
            download_url(RELEASE_BASE_URL, "0.1.5", SUMS_FILE),
            "https://github.com/Crosery/uniflo/releases/download/v0.1.5/SHA256SUMS"
        );
    }

    #[test]
    fn sums_file_lookup() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  uniflo-0.1.5-aarch64-apple-darwin.tar.gz\n{b} *uniflo-0.1.5-x86_64-pc-windows-msvc.zip\nnot a line\nabc  short\n"
        );
        assert_eq!(listed_sum(&sums, "uniflo-0.1.5-aarch64-apple-darwin.tar.gz"), Some(a));
        assert_eq!(listed_sum(&sums, "uniflo-0.1.5-x86_64-pc-windows-msvc.zip"), Some("b".repeat(64)));
        assert_eq!(listed_sum(&sums, "uniflo-0.1.5-aarch64-apple-darwin"), None);
        assert_eq!(listed_sum(&sums, "short"), None);
    }

    #[test]
    fn swap_replace_and_aside() {
        let d = tmp("swap");
        let exe = d.join("uniflo-bin");
        let new = d.join("new-build");
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();

        swap(&new, &exe, Swap::Replace).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert!(!aside_path(&exe).exists());

        std::fs::write(&new, "newer").unwrap();
        swap(&new, &exe, Swap::Aside).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "newer");
        assert_eq!(aside_path(&exe), d.join("uniflo-bin.old"));
        assert_eq!(std::fs::read_to_string(aside_path(&exe)).unwrap(), "new");
        // a leftover from an earlier upgrade does not block the next one
        std::fs::write(&new, "newest").unwrap();
        swap(&new, &exe, Swap::Aside).unwrap();
        assert_eq!(std::fs::read_to_string(aside_path(&exe)).unwrap(), "newer");
        remove_aside(&exe);
        assert!(!aside_path(&exe).exists());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "newest");

        // a failed swap leaves the executable and no staged copy behind
        let missing = d.join("gone/uniflo");
        assert!(swap(&new, &missing, Swap::Aside).is_err());
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 2, "{left:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn remove_aside_preserves_unrelated_names_and_unicode() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("uniflo.exe");
        let owned = ["uniflo.exe.old", "uniflo.exe.old-172990-1234-1"];
        let unrelated = ["uniflo.exe.old.bak", "uniflo.exe.old-dead", "other.exe.old-1-2-3", "甲甲甲甲甲甲甲甲"];
        for name in owned.iter().chain(&unrelated) {
            std::fs::write(d.path().join(name), name).unwrap();
        }
        remove_aside(&exe);
        for name in owned {
            assert!(!d.path().join(name).exists());
        }
        for name in unrelated {
            assert_eq!(std::fs::read_to_string(d.path().join(name)).unwrap(), name);
        }
    }

    #[test]
    fn swap_aside_repeated_when_legacy_backup_cannot_be_removed() {
        let d = tmp("swap-held");
        let exe = d.join("uniflo-bin");
        let new1 = d.join("new1");
        let new2 = d.join("new2");
        std::fs::write(&exe, "v1").unwrap();
        std::fs::write(&new1, "v2").unwrap();
        std::fs::write(&new2, "v3").unwrap();

        // Swap 1: creates legacy .old
        swap(&new1, &exe, Swap::Aside).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        let legacy = aside_path(&exe);
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "v1");

        // Simulate legacy .old being locked / cannot be removed.
        // std::fs::remove_file fails on directories across platforms.
        std::fs::remove_file(&legacy).unwrap();
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("locked_indicator"), "held").unwrap();

        // Swap 2: legacy cannot be removed, should fallback to unique generated backup
        swap(&new2, &exe, Swap::Aside).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v3");

        // The directory simulating held legacy is still intact
        assert!(legacy.join("locked_indicator").exists());

        // Find the generated backup that received v2
        let entries: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| is_recognized_backup(&exe, &e.file_name().to_string_lossy()))
            .filter(|e| e.path() != legacy)
            .collect();
        assert_eq!(entries.len(), 1, "expected one generated backup: {entries:?}");
        assert_eq!(std::fs::read_to_string(entries[0].path()).unwrap(), "v2");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn swap_aside_windows_file_locking() {
        let d = tmp("win-lock");
        let exe = d.join("uniflo.exe");
        let new1 = d.join("new1.exe");
        let new2 = d.join("new2.exe");
        std::fs::write(&exe, "v1").unwrap();
        std::fs::write(&new1, "v2").unwrap();
        std::fs::write(&new2, "v3").unwrap();

        // First swap: creates uniflo.exe.old
        swap(&new1, &exe, Swap::Aside).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        let legacy = aside_path(&exe);
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "v1");

        use std::os::windows::fs::OpenOptionsExt;
        let held = std::fs::OpenOptions::new().read(true).share_mode(3).open(&legacy).unwrap();
        swap(&new2, &exe, Swap::Aside).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v3");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "v1");

        // Allow copying into the staged file, but deny its final rename to force a real rollback.
        let staged = d.join(format!(".uniflo-new-{}", std::process::id()));
        std::fs::write(&staged, "staged").unwrap();
        let held_staged = std::fs::OpenOptions::new().read(true).share_mode(3).open(&staged).unwrap();
        assert!(swap(&new1, &exe, Swap::Aside).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v3");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "v1");
        drop(held_staged);
        drop(held);
        remove_aside(&exe);
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_source_hands_over_to_the_next() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let good = format!("http://{}/releases", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut s in l.incoming().map_while(Result::ok) {
                let mut req = [0u8; 4096];
                let n = s.read(&mut req).unwrap_or(0);
                let hit = String::from_utf8_lossy(&req[..n]).starts_with("GET /releases/download/v1.2.3/SHA256SUMS ");
                let (status, body) = if hit { ("200 OK", "sums") } else { ("404 Not Found", "") };
                let _ =
                    write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bases = vec![format!("http://127.0.0.1:{port}/releases"), good.clone()];
        let work = Scratch::new().unwrap();
        let dest = work.0.join(SUMS_FILE);
        assert_eq!(fetch_any(&bases, "1.2.3", SUMS_FILE, &dest).unwrap(), good);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "sums");
        let err = fetch_any(&bases[..1], "1.2.3", SUMS_FILE, &dest).unwrap_err();
        assert!(format!("{err:#}").contains(&format!("127.0.0.1:{port}")), "{err:#}");
    }

    #[test]
    fn record_roundtrip() {
        let d = tmp("record");
        let p = d.join("cfg/install.json");
        let mut r = record(&d.join("bin/uniflo"));
        r.save(&p).unwrap();
        assert_eq!(InstallRecord::load(&p), Some(r.clone()));
        r.version = "0.1.5".into();
        r.save(&p).unwrap();
        assert_eq!(InstallRecord::load(&p).unwrap().version, "0.1.5");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["method", "target", "version", "path"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
