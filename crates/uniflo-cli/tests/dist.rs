//! Prebuilt distribution end to end (ADR-0009): `scripts/package.sh` packs the real `uniflo`
//! binary, a local HTTP server stands in for GitHub Releases and the crates.io index,
//! `scripts/install.sh` installs into a temporary directory, and the installed binary upgrades
//! itself. Every path, the config dir and `TMPDIR` are temporary; nothing goes to the network.
#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use uniflo_core::install::{asset_name, host_target};

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const NEXT: &str = "9.9.9";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target() -> &'static str {
    host_target().expect("a release target for this platform")
}

/// Static files under `root`: `/releases/...` mirrors GitHub Releases, `/index/uniflo` the
/// crates.io sparse index.
fn serve(root: PathBuf) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut rd = BufReader::new(&s);
            let mut first = String::new();
            let _ = rd.read_line(&mut first);
            let mut line = String::new();
            while rd.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            let path = first.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_owned();
            let file = root.join(path);
            let (status, body) = match std::fs::read(&file) {
                Ok(b) if file.is_file() => (200, b),
                _ => (404, b"not found".to_vec()),
            };
            let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = (&s).write_all(head.as_bytes()).and_then(|_| (&s).write_all(&body));
        }
    });
    url
}

fn package(version: &str, bin: &Path, out: &Path) -> PathBuf {
    let o = Command::new("bash")
        .arg(repo().join("scripts/package.sh"))
        .args([version, target()])
        .arg(bin)
        .arg(out)
        .output()
        .unwrap();
    assert!(o.status.success(), "package.sh: {}", String::from_utf8_lossy(&o.stderr));
    PathBuf::from(String::from_utf8(o.stdout).unwrap().trim())
}

/// The real binary packed once per test run (it is tens of MB).
fn current_package() -> &'static Path {
    static PKG: OnceLock<PathBuf> = OnceLock::new();
    PKG.get_or_init(|| package(VERSION, Path::new(BIN), &Path::new(env!("CARGO_TARGET_TMPDIR")).join("dist-package")))
}

struct Release {
    dir: tempfile::TempDir,
    url: String,
}

impl Release {
    fn new() -> Release {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(dir.path().to_owned());
        Release { dir, url }
    }

    fn base(&self) -> String {
        format!("{}/releases", self.url)
    }

    fn version_dir(&self, version: &str) -> PathBuf {
        self.dir.path().join(format!("releases/download/v{version}"))
    }

    /// Publish `pkg` as `version`, with its SHA256SUMS; `latest` also updates `/latest/download/`.
    fn publish(&self, version: &str, pkg: &Path, latest: bool) {
        let d = self.version_dir(version);
        std::fs::create_dir_all(&d).unwrap();
        let name = asset_name(version, target());
        std::fs::copy(pkg, d.join(&name)).unwrap();
        let sum = uniflo_core::archive::sha256_file(&d.join(&name)).unwrap();
        std::fs::write(d.join("SHA256SUMS"), format!("{sum}  {name}\n")).unwrap();
        if latest {
            let l = self.dir.path().join("releases/latest/download");
            std::fs::create_dir_all(&l).unwrap();
            std::fs::copy(d.join("SHA256SUMS"), l.join("SHA256SUMS")).unwrap();
        }
    }

    fn served(&self, version: &str) -> PathBuf {
        self.version_dir(version).join(asset_name(version, target()))
    }

    fn sums(&self, version: &str) -> PathBuf {
        self.version_dir(version).join("SHA256SUMS")
    }

    /// The crates.io sparse index lists `versions`.
    fn index(&self, versions: &[&str]) {
        let p = self.dir.path().join("index/uniflo");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let lines: Vec<String> =
            versions.iter().map(|v| format!("{{\"name\":\"uniflo\",\"vers\":\"{v}\",\"yanked\":false}}")).collect();
        std::fs::write(p, lines.join("\n") + "\n").unwrap();
    }
}

/// A temporary machine: home, config dir, install dir and TMPDIR.
struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    fn new() -> Machine {
        let m = Machine { dir: tempfile::tempdir().unwrap() };
        for d in ["home", "tmp"] {
            std::fs::create_dir_all(m.dir.path().join(d)).unwrap();
        }
        m
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn cfg(&self) -> PathBuf {
        self.path("cfg")
    }

    fn record(&self) -> Option<Value> {
        serde_json::from_slice(&std::fs::read(self.cfg().join("install.json")).ok()?).ok()
    }

    fn cmd(&self, program: impl AsRef<std::ffi::OsStr>, rel: &Release) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.path("home"))
            .env("UNIFLO_HOME", self.path("home"))
            .env("UNIFLO_CONFIG_DIR", self.cfg())
            .env("TMPDIR", self.path("tmp"))
            .env("UNIFLO_RELEASE_BASE_URL", rel.base())
            .env("UNIFLO_UPDATE_INDEX_URL", format!("{}/index/uniflo", rel.url))
            .stdin(Stdio::null());
        c
    }

    fn install(&self, rel: &Release, dir: &str, extra: &[(&str, &str)], args: &[&str]) -> Output {
        let mut c = self.cmd("sh", rel);
        c.arg(repo().join("scripts/install.sh")).args(args).env("UNIFLO_INSTALL_DIR", self.path(dir));
        for (k, v) in extra {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn uniflo(&self, exe: &Path, rel: &Release, args: &[&str]) -> Output {
        self.cmd(exe, rel).args(args).output().unwrap()
    }

    fn tmp_left(&self) -> Vec<String> {
        listing(&self.path("tmp"))
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn version_of(exe: &Path) -> String {
    text(&Command::new(exe).arg("--version").output().unwrap().stdout).trim().to_owned()
}

fn check_json(m: &Machine, exe: &Path, rel: &Release) -> (Value, Output) {
    let o = m.uniflo(exe, rel, &["update", "--check", "--json"]);
    (serde_json::from_slice(&o.stdout).unwrap_or(Value::Null), o)
}

/// A stand-in for the next release: it only has to answer `--version`.
fn next_binary(dir: &Path) -> PathBuf {
    let p = dir.join("uniflo-next");
    std::fs::write(&p, format!("#!/bin/sh\necho 'uniflo {NEXT}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn install_script_installs_records_and_skips_setup_off_a_terminal() {
    let rel = Release::new();
    rel.publish(VERSION, current_package(), true);
    let m = Machine::new();

    let o = m.install(&rel, "bin", &[], &[]);
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out}\n{}", text(&o.stderr));
    let exe = m.path("bin/uniflo");
    assert_eq!(version_of(&exe), format!("uniflo {VERSION}"));
    assert_eq!(listing(&m.path("bin")), ["uniflo"]);
    let rec = m.record().expect("install.json");
    assert_eq!(rec["method"], "binary");
    assert_eq!(rec["target"], target());
    assert_eq!(rec["version"], VERSION);
    assert_eq!(rec["path"].as_str().map(PathBuf::from), Some(std::fs::canonicalize(&exe).unwrap()));
    assert!(out.contains("跳过 uniflo setup"), "{out}");
    assert!(!m.cfg().join("setup.json").exists(), "setup must not run off a terminal");
    assert!(out.contains("不在 PATH 中"), "{out}");
    assert!(m.tmp_left().is_empty(), "temp files left: {:?}", m.tmp_left());

    // the record is the one `uniflo update` recognises
    rel.index(&[VERSION]);
    let (j, _) = check_json(&m, &exe, &rel);
    assert_eq!(j["method"], "binary", "{j}");

    // --no-setup and UNIFLO_NO_SETUP=1 are accepted; unknown flags are refused
    assert!(m.install(&rel, "bin", &[], &["--no-setup"]).status.success());
    assert!(m.install(&rel, "bin", &[("UNIFLO_NO_SETUP", "1")], &[]).status.success());
    let bad = m.install(&rel, "bin", &[], &["--yes"]);
    assert!(!bad.status.success());
    assert_eq!(listing(&m.path("bin")), ["uniflo"]);
}

#[test]
fn install_script_follows_uniflo_home_for_the_record() {
    let rel = Release::new();
    rel.publish(VERSION, current_package(), true);
    let m = Machine::new();
    let o = m
        .cmd("sh", &rel)
        .env_remove("UNIFLO_CONFIG_DIR")
        .env("UNIFLO_INSTALL_DIR", m.path("bin"))
        .env("UNIFLO_VERSION", format!("v{VERSION}"))
        .arg(repo().join("scripts/install.sh"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    // the binary finds the record the script wrote under the re-rooted config dir
    rel.index(&[VERSION]);
    let o = m
        .cmd(m.path("bin/uniflo"), &rel)
        .env_remove("UNIFLO_CONFIG_DIR")
        .args(["update", "--check", "--json"])
        .output()
        .unwrap();
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["method"], "binary", "{j}");
    let under_home: Vec<_> = walk(&m.path("home")).into_iter().filter(|p| p.ends_with("uniflo/install.json")).collect();
    assert_eq!(under_home.len(), 1, "{under_home:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn install_script_aborts_on_a_checksum_mismatch_and_leaves_nothing() {
    let rel = Release::new();
    rel.publish(VERSION, current_package(), true);
    let mut bytes = std::fs::read(rel.served(VERSION)).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(rel.served(VERSION), &bytes).unwrap();
    let m = Machine::new();

    let o = m.install(&rel, "fresh/bin", &[], &[]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(err.contains("校验失败"), "{err}");
    assert!(!m.path("fresh").exists(), "install dir must not be created");
    assert!(m.record().is_none());
    assert!(m.tmp_left().is_empty(), "temp files left: {:?}", m.tmp_left());

    // over an existing install: the old executable stays byte for byte, nothing is added
    std::fs::create_dir_all(m.path("bin")).unwrap();
    std::fs::write(m.path("bin/uniflo"), "previous").unwrap();
    let o = m.install(&rel, "bin", &[], &[]);
    assert!(!o.status.success());
    assert_eq!(std::fs::read_to_string(m.path("bin/uniflo")).unwrap(), "previous");
    assert_eq!(listing(&m.path("bin")), ["uniflo"]);
    assert!(m.record().is_none());
    assert!(m.tmp_left().is_empty());
}

#[test]
fn binary_install_upgrades_itself_and_failures_change_nothing() {
    let rel = Release::new();
    rel.publish(VERSION, current_package(), true);
    let m = Machine::new();
    assert!(m.install(&rel, "bin", &[], &[]).status.success());
    let exe = m.path("bin/uniflo");
    let before = std::fs::read(&exe).unwrap();
    rel.index(&[VERSION, NEXT]);
    let next = package(NEXT, &next_binary(&m.path("tmp")), &m.path("next"));
    rel.publish(NEXT, &next, false);

    let unchanged = |o: &Output, why: &str| {
        let err = text(&o.stderr);
        assert!(!o.status.success(), "{why}: should fail");
        assert!(err.contains("升级失败") && err.contains(why), "{why}: {err}");
        assert!(std::fs::read(&exe).unwrap() == before, "{why}: executable changed");
        assert_eq!(listing(&m.path("bin")), ["uniflo"], "{why}");
        assert_eq!(m.record().unwrap()["version"], VERSION, "{why}");
    };

    // wrong checksum in SHA256SUMS
    let good_sums = std::fs::read_to_string(rel.sums(NEXT)).unwrap();
    std::fs::write(rel.sums(NEXT), format!("{}  {}\n", "0".repeat(64), asset_name(NEXT, target()))).unwrap();
    unchanged(&m.uniflo(&exe, &rel, &["update"]), "checksum mismatch");
    // package missing
    std::fs::write(rel.sums(NEXT), &good_sums).unwrap();
    std::fs::rename(rel.served(NEXT), m.path("next/held")).unwrap();
    unchanged(&m.uniflo(&exe, &rel, &["update"]), "download");
    // not an archive, checksum consistent
    std::fs::write(rel.served(NEXT), "not a tarball").unwrap();
    let sum = uniflo_core::archive::sha256_file(&rel.served(NEXT)).unwrap();
    std::fs::write(rel.sums(NEXT), format!("{sum}  {}\n", asset_name(NEXT, target()))).unwrap();
    unchanged(&m.uniflo(&exe, &rel, &["update"]), "unpack");

    // the real package
    std::fs::rename(m.path("next/held"), rel.served(NEXT)).unwrap();
    std::fs::write(rel.sums(NEXT), &good_sums).unwrap();
    let o = m.uniflo(&exe, &rel, &["update"]);
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out}\n{}", text(&o.stderr));
    assert!(out.contains(&format!("已安装 uniflo {NEXT}")), "{out}");
    assert!(!out.contains("已重启"), "launchd must not be restarted for a temporary install: {out}");
    assert_eq!(version_of(&exe), format!("uniflo {NEXT}"));
    assert_eq!(m.record().unwrap()["version"], NEXT);
    assert_eq!(listing(&m.path("bin")), ["uniflo"]);
    assert!(m.tmp_left().iter().all(|n| n == "uniflo-next"), "{:?}", m.tmp_left());
}

#[test]
fn update_reports_the_install_method() {
    let rel = Release::new();
    rel.index(&[VERSION, NEXT]);
    let m = Machine::new();
    let place = |rel_path: &str| {
        let p = m.path(rel_path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::copy(BIN, &p).unwrap();
        p
    };

    // $CARGO_HOME/bin and ~/.cargo/bin
    let cargo = place("cargo/bin/uniflo");
    let o =
        m.cmd(&cargo, &rel).env("CARGO_HOME", m.path("cargo")).args(["update", "--check", "--json"]).output().unwrap();
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["method"], "cargo", "{j}");
    assert_eq!(j["latest"], NEXT);
    assert_eq!(o.status.code(), Some(10), "update available");
    let home_cargo = place("home/.cargo/bin/uniflo");
    assert_eq!(check_json(&m, &home_cargo, &rel).0["method"], "cargo");

    // a record naming this executable
    let binary = place("bin/uniflo");
    let rec = serde_json::json!({"method": "binary", "target": target(), "version": VERSION, "path": binary});
    std::fs::create_dir_all(m.cfg()).unwrap();
    std::fs::write(m.cfg().join("install.json"), rec.to_string()).unwrap();
    assert_eq!(check_json(&m, &binary, &rel).0["method"], "binary");

    // neither: unknown, both ways printed, and `uniflo update` changes nothing
    let other = place("other/uniflo");
    let (j, o) = check_json(&m, &other, &rel);
    assert_eq!(j["method"], "unknown", "{j}");
    let err = text(&o.stderr);
    assert!(err.contains("cargo install uniflo --force") && err.contains("install.sh | sh"), "{err}");
    let o = m.uniflo(&other, &rel, &["update"]);
    let out = text(&o.stdout);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(out.contains(&format!("cargo install uniflo --force --version {NEXT}")), "{out}");
    assert!(out.contains(&format!("install.sh | UNIFLO_VERSION={NEXT} sh")), "{out}");
    assert!(std::fs::read(&other).unwrap() == std::fs::read(BIN).unwrap());
}
